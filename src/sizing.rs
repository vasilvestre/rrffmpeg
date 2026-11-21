//! Size a host's pools from its own journal.
//!
//! Pure: samples in, cap out. Nothing here reads a file or spawns a process,
//! so every rule is testable without a state dir (ADR 0033).
//!
//! The measure is the 5th percentile of the solo speeds of one content class.
//! A speed of 1.43x means the encoder keeps up with realtime with 43% to
//! spare, so one job is safe and two are not. `HEADROOM` is that margin made
//! explicit: a host whose p05 is over 1.3x could take a second job before the
//! first one starts to stutter, and ten finishes that all clear 1.3x are the
//! evidence that it does.

use crate::Kind;
use crate::state::{Event, Record, Stats};
use std::collections::BTreeMap;

/// A cap never goes above this, however fast the host measured: a decode, the
/// audio, the muxer and the disk share the box with the encoder.
pub const MAX_JOBS: usize = 4;
/// A job needs this much room over realtime before a second one may run.
pub const HEADROOM: f64 = 1.3;
/// How many finished jobs of a host the sizing reads.
pub const WINDOW: usize = 200;
/// A shorter job says nothing about sustained speed: the first seconds are the
/// encoder warming up, and a 10 s thumbnail reports that warm-up.
pub const MIN_DURATION_US: u64 = 60_000_000;
/// This many finishes in a row at `HEADROOM` or better raise the cap by one.
pub const RAISE_AFTER: usize = 10;
/// This many finishes in a row under realtime lower it at once.
pub const LOWER_AFTER: usize = 3;

/// What the cap of a class depends on: the source codec and scale, the output
/// codec, and whether the source needs tone mapping. Two 1080p HEVC sources of
/// the same length cost the same whatever their bitrate, measured.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub src_codec: String,
    /// Source height in pixels, as ffmpeg reported it. The class is the
    /// content, and the label an operator reads must be the height the source
    /// has: bucketing 1080 into "10 hundreds" printed "1000p".
    pub src_h: u32,
    pub dst_codec: String,
    /// An HDR source is the one that pays for tone mapping.
    pub tonemap: bool,
}

impl Key {
    /// The class of a finished job, or `None` when its sample says nothing
    /// about the source or the output, which is what an old journal holds.
    pub fn of(stats: &Stats) -> Option<Key> {
        Some(Key {
            src_codec: stats.src_codec.clone()?,
            src_h: stats.src_h?,
            dst_codec: stats.dst_codec.clone()?,
            tonemap: stats.src_hdr.is_some(),
        })
    }

    /// The words a `Sized` line and `rrffmpeg-stats` use.
    pub fn label(&self) -> String {
        let mut label = format!("{} {}p {}", self.src_codec, self.src_h, self.dst_codec);
        if self.tonemap {
            label.push_str(" tonemap");
        }
        label
    }
}

/// One finished transcode, ready to size from.
#[derive(Debug, Clone)]
pub struct Sample {
    pub key: Key,
    /// Times realtime while `in_flight` jobs ran on the host.
    pub speed: f64,
    pub in_flight: usize,
}

impl Sample {
    /// What this job would have reached alone. A neighbour slows an encode
    /// roughly in proportion to how many share the encoder, so two jobs at
    /// 0.7x each say the host does 1.4x for one.
    pub fn solo(&self) -> f64 {
        if self.in_flight > 1 {
            self.speed * self.in_flight as f64
        } else {
            self.speed
        }
    }
}

/// One cap, with the numbers behind it so the caller can log the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Size {
    /// The class this cap is for, `None` when the host has no sample yet.
    pub key: Option<Key>,
    pub cap: usize,
    /// Finishes the percentile was read from.
    pub samples: usize,
    pub p05: Option<f64>,
    /// The cap before the hysteresis move, so a change has a reason.
    pub previous: usize,
}

impl Size {
    /// No sample: one job, the safe answer for a host nobody measured.
    pub fn none() -> Size {
        Size {
            key: None,
            cap: 1,
            samples: 0,
            p05: None,
            // 0, so the caller records the cold start with `samples 0`.
            previous: 0,
        }
    }
}

/// The finished transcode jobs of each host, oldest first, at most `WINDOW`
/// per host. `Started` carries the kind and the occupancy, `Finished` the host
/// and the numbers; a tail that lost the start line of a long job skips it.
pub fn samples(records: &[Record]) -> BTreeMap<String, Vec<Sample>> {
    let mut started: BTreeMap<&str, (&str, usize)> = BTreeMap::new();
    let mut out: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
    for record in records {
        match &record.ev {
            Event::Started {
                job,
                job_kind,
                in_flight,
                ..
            } => {
                started.insert(job.as_str(), (job_kind.as_str(), *in_flight));
            }
            Event::Finished {
                job,
                host,
                dur_us,
                stats,
                ..
            } => {
                let Some((kind, in_flight)) = started.remove(job.as_str()) else {
                    continue;
                };
                if kind != Kind::Transcode.name() || *dur_us <= MIN_DURATION_US {
                    continue;
                }
                let Some(speed) = stats.speed.filter(|speed| *speed > 0.0) else {
                    continue;
                };
                let Some(key) = Key::of(stats) else {
                    continue;
                };
                let host = out.entry(host.clone()).or_default();
                host.push(Sample {
                    key,
                    speed,
                    in_flight,
                });
                if host.len() > WINDOW {
                    host.remove(0);
                }
            }
            _ => {}
        }
    }
    out
}

/// The classes of a host, in a stable order.
pub fn classes(samples: &[Sample]) -> Vec<Key> {
    let mut keys: BTreeMap<&Key, ()> = BTreeMap::new();
    for sample in samples {
        keys.insert(&sample.key, ());
    }
    keys.into_keys().cloned().collect()
}

/// Nearest-rank percentile of an unsorted list. Empty and NaN are handled by
/// the caller: every sample here has a positive speed.
pub fn percentile(values: &mut [f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let len = values.len();
    let rank = (fraction * len as f64).ceil() as usize;
    values[rank.clamp(1, len) - 1]
}

/// 5th percentile of the solo speeds of one class, and how many finishes it
/// came from. Solo samples win: a job run alone measured the host, a job run
/// beside another one measured a queue.
pub fn solo_p05(samples: &[Sample], key: &Key) -> Option<(f64, usize)> {
    let of_class: Vec<&Sample> = samples.iter().filter(|sample| &sample.key == key).collect();
    if of_class.is_empty() {
        return None;
    }
    let mut solo: Vec<f64> = of_class
        .iter()
        .filter(|sample| sample.in_flight == 1)
        .map(|sample| sample.speed)
        .collect();
    let mut scaled: Vec<f64> = of_class.iter().map(|sample| sample.solo()).collect();
    let values = if solo.is_empty() {
        &mut scaled
    } else {
        &mut solo
    };
    let count = values.len();
    Some((percentile(values, 0.05), count))
}

/// The most jobs a host may run, from its core count. Half the cores leaves
/// the decode and the audio somewhere to run, and 4 is the ceiling because a
/// fifth transcode adds a queue, not pixels.
pub fn ceiling(cpus: usize) -> usize {
    (cpus / 2).clamp(1, MAX_JOBS)
}

/// The Light pool under `"auto"`: enough for a thumbnail, a trickplay scan and
/// a probe at once, never more than four. Fixed, because a probe is cheap and
/// its own speed says nothing about the GPU.
pub fn light_cap(cpus: usize) -> usize {
    (cpus / 2).clamp(2, MAX_JOBS)
}

/// The cap the measured speed alone allows: one job per 1.3x of headroom.
pub fn base_cap(p05: f64, cpus: usize) -> usize {
    let jobs = (p05 / HEADROOM).floor();
    let jobs = if jobs.is_finite() && jobs >= 1.0 {
        jobs as usize
    } else {
        1
    };
    jobs.clamp(1, ceiling(cpus))
}

/// Move the cap by one for the last finishes, never more.
///
/// Three finishes under realtime say the second job is stealing from the
/// first, so the cap drops at once. Ten finishes at 1.3x or better say the
/// next job would fit, so it rises. Both read the same tail as the percentile,
/// so two processes compute the same answer and no file has to hold it.
pub fn hysteresis(samples: &[Sample], key: &Key, cap: usize, top: usize) -> usize {
    let of_class: Vec<&Sample> = samples.iter().filter(|sample| &sample.key == key).collect();
    let tail = |count: usize| -> Option<Vec<f64>> {
        if of_class.len() < count {
            return None;
        }
        Some(
            of_class[of_class.len() - count..]
                .iter()
                .map(|sample| sample.solo())
                .collect(),
        )
    };
    if let Some(last) = tail(LOWER_AFTER)
        && last.iter().all(|speed| *speed < 1.0)
    {
        return cap.saturating_sub(1).max(1);
    }
    if let Some(last) = tail(RAISE_AFTER)
        && last.iter().all(|speed| *speed >= HEADROOM)
    {
        return (cap + 1).min(top);
    }
    cap
}

/// The cap of one class, with the numbers behind it. `None` when this class
/// has no sample.
pub fn size_class(samples: &[Sample], key: &Key, cpus: usize) -> Option<Size> {
    let (p05, count) = solo_p05(samples, key)?;
    let base = base_cap(p05, cpus);
    Some(Size {
        key: Some(key.clone()),
        cap: hysteresis(samples, key, base, ceiling(cpus)),
        samples: count,
        p05: Some(p05),
        previous: base,
    })
}

/// The last cap a `Sized` line recorded for this host and class, 0 when the
/// tail holds none. The comparison keeps the audit trail to real changes and
/// gives every change a `previous`.
pub fn last_sized(records: &[Record], host: &str, class: &str) -> usize {
    records
        .iter()
        .rev()
        .find_map(|record| match &record.ev {
            Event::Sized {
                host: line_host,
                class: line_class,
                cap,
                ..
            } if line_host == host && line_class == class => Some(*cap),
            _ => None,
        })
        .unwrap_or(0)
}

/// The heavy cap of a host: its smallest class cap, because the host has to
/// serve the class it encodes worst. One line naming that class is what the
/// caller logs when there was no sample at all.
pub fn heavy(samples: &[Sample], cpus: usize) -> Size {
    let mut best: Option<Size> = None;
    for key in classes(samples) {
        let Some(size) = size_class(samples, &key, cpus) else {
            continue;
        };
        if best.as_ref().is_none_or(|current| size.cap < current.cap) {
            best = Some(size);
        }
    }
    best.unwrap_or_else(Size::none)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Record, Stats};

    fn key() -> Key {
        Key {
            src_codec: "hevc".into(),
            src_h: 1080,
            dst_codec: "h264".into(),
            tonemap: false,
        }
    }

    fn sample(speed: f64, in_flight: usize) -> Sample {
        Sample {
            key: key(),
            speed,
            in_flight,
        }
    }

    fn finished(seq: u64, job: &str, speed: f64, in_flight: usize) -> Vec<Record> {
        let started = Record {
            seq,
            ts: 0,
            ev: Event::Started {
                job: job.into(),
                host: "worker".into(),
                elapsed_us: 0,
                program: "ffmpeg".into(),
                in_flight,
                class: "heavy".into(),
                pool: "heavy".into(),
                job_kind: "transcode".into(),
                input: "/m/ep.mkv".into(),
            },
        };
        let stats = Stats {
            speed: Some(speed),
            src_codec: Some("hevc".into()),
            src_h: Some(1080),
            dst_codec: Some("h264".into()),
            ..Stats::default()
        };
        let done = Record {
            seq: seq + 1,
            ts: 0,
            ev: Event::Finished {
                job: job.into(),
                code: 0,
                host: "worker".into(),
                dur_us: 120_000_000,
                stats: Box::new(stats),
            },
        };
        vec![started, done]
    }

    #[test]
    fn a_host_with_no_sample_gets_one_job() {
        let size = heavy(&[], 16);
        assert_eq!(size.cap, 1);
        assert_eq!(size.samples, 0);
        assert!(size.key.is_none(), "{size:?}");
        assert!(size.p05.is_none(), "{size:?}");
    }

    #[test]
    fn a_solo_speed_of_0_8_keeps_one_job() {
        let size = heavy(&[sample(0.8, 1)], 16);
        assert_eq!(size.cap, 1, "{size:?}");
        assert_eq!(size.samples, 1);
        assert_eq!(size.p05, Some(0.8));
    }

    /// 1.43x is one job with headroom, not two: the second one is what ten
    /// finishes at 1.3x have to prove.
    #[test]
    fn a_solo_speed_of_1_43_keeps_one_job_until_ten_good_finishes() {
        let one = vec![sample(1.43, 1)];
        assert_eq!(heavy(&one, 16).cap, 1);
        let ten: Vec<Sample> = (0..RAISE_AFTER).map(|_| sample(1.43, 1)).collect();
        let size = heavy(&ten, 16);
        assert_eq!(size.cap, 2, "{size:?}");
        assert_eq!(size.previous, 1, "{size:?}");
    }

    #[test]
    fn a_solo_speed_of_62_is_clamped_to_the_ceiling() {
        let samples = vec![sample(62.0, 1)];
        assert_eq!(heavy(&samples, 16).cap, 4);
        assert_eq!(heavy(&samples, 4).cap, 2);
        assert_eq!(heavy(&samples, 2).cap, 1);
    }

    #[test]
    fn ten_good_finishes_raise_the_cap_and_nine_do_not() {
        let mut samples: Vec<Sample> = (0..RAISE_AFTER - 1).map(|_| sample(2.0, 1)).collect();
        assert_eq!(hysteresis(&samples, &key(), 1, 4), 1);
        samples.push(sample(2.0, 1));
        assert_eq!(hysteresis(&samples, &key(), 1, 4), 2);
        // The ceiling still wins.
        assert_eq!(hysteresis(&samples, &key(), 4, 4), 4);
    }

    #[test]
    fn three_slow_finishes_lower_the_cap() {
        let samples: Vec<Sample> = (0..LOWER_AFTER).map(|_| sample(0.9, 1)).collect();
        assert_eq!(hysteresis(&samples, &key(), 2, 4), 1);
        // Two are not enough.
        assert_eq!(hysteresis(&samples[..2], &key(), 2, 4), 2);
        // And the floor is one.
        assert_eq!(hysteresis(&samples, &key(), 1, 4), 1);
    }

    #[test]
    fn a_neighbour_slowed_job_is_scaled_back_to_solo() {
        let pair = vec![sample(0.7, 2), sample(0.75, 2)];
        let size = heavy(&pair, 16);
        assert_eq!(size.samples, 2);
        assert_eq!(size.p05, Some(1.4));
        assert_eq!(size.cap, 1);
        // A solo sample is used as is, and wins over the pair.
        let mixed = vec![sample(0.7, 2), sample(3.0, 1)];
        let size = heavy(&mixed, 16);
        assert_eq!(size.samples, 1, "{size:?}");
        assert_eq!(size.p05, Some(3.0));
    }

    #[test]
    fn each_content_class_has_its_own_cap() {
        let mut tonemapped = sample(0.8, 1);
        tonemapped.key.tonemap = true;
        let label = tonemapped.key.label();
        let samples = vec![sample(2.6, 1), tonemapped];
        let size = heavy(&samples, 16);
        assert_eq!(size.cap, 1, "{size:?}");
        assert_eq!(size.key.as_ref().map(Key::label), Some(label));
        assert_eq!(size.p05, Some(0.8));
        // The faster class alone allows two.
        assert_eq!(heavy(&samples[..1], 16).cap, 2);
    }

    #[test]
    fn the_light_cap_never_drops_below_two() {
        assert_eq!(light_cap(1), 2);
        assert_eq!(light_cap(4), 2);
        assert_eq!(light_cap(8), 4);
        assert_eq!(light_cap(64), 4);
        assert_eq!(ceiling(1), 1);
        assert_eq!(ceiling(3), 1);
        assert_eq!(ceiling(64), 4);
    }

    #[test]
    fn a_short_or_kindless_job_is_not_a_sample() {
        let records = finished(1, "a", 2.0, 1);
        let mut short = records.clone();
        if let Event::Finished { dur_us, .. } = &mut short[1].ev {
            *dur_us = 30_000_000;
        }
        assert!(samples(&short).is_empty());

        let mut probe = records.clone();
        if let Event::Started { job_kind, .. } = &mut probe[0].ev {
            *job_kind = "probe".into();
        }
        assert!(samples(&probe).is_empty());

        let mut old = records.clone();
        if let Event::Finished { stats, .. } = &mut old[1].ev {
            stats.speed = None;
        }
        assert!(samples(&old).is_empty());

        let one = samples(&records);
        assert_eq!(one.get("worker").map(Vec::len), Some(1));
    }

    #[test]
    fn only_the_last_window_is_read() {
        let mut records = Vec::new();
        let mut seq = 1;
        for index in 0..WINDOW + 10 {
            // The first ten are slow, the rest fast.
            let speed = if index < 10 { 0.5 } else { 4.0 };
            records.extend(finished(seq, &format!("j{index}"), speed, 1));
            seq += 2;
        }
        let samples = samples(&records);
        let host = samples.get("worker").expect("samples");
        assert_eq!(host.len(), WINDOW);
        assert!(host.iter().all(|sample| sample.speed == 4.0));
    }
}
