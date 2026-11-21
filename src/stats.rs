//! Read the journal back and summarize what it says.
//!
//! rrffmpeg is one process per job, so there is no daemon to scrape and no
//! port to open. After the job ends, the journal is the only place its numbers
//! exist. This module turns those lines into the two figures that matter: how
//! long the dispatcher took before the worker started, and how long the job
//! itself ran.

use crate::Error;
use crate::Result;
use crate::state::{Event, records};
use std::collections::BTreeMap;
use std::path::Path;

/// Microseconds at a rank, nearest rank, on a sorted slice.
pub fn percentile(sorted: &[u64], fraction: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((fraction * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[index - 1]
}

#[derive(Debug, Default, Clone)]
pub struct HostTally {
    pub jobs: usize,
    pub failed: usize,
    pub down: usize,
    pub up: usize,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub host: String,
    /// Microseconds from process start to the moment the job went out.
    pub dispatch_us: u64,
    pub started_ts: u64,
    /// Set once a Finished event arrives.
    pub duration_us: Option<u64>,
    pub code: Option<i32>,
    /// `light` or `heavy`, as the dispatcher classified the job.
    pub class: String,
    /// What the job does, for example `trickplay` or `transcode`. `unknown`
    /// when the journal predates the field.
    pub kind: String,
    /// The input path with `file:` stripped, empty when the job had no `-i`.
    pub input: String,
    pub reaped: bool,
    /// Microseconds the job spent parked: Jellyfin paused the stream, so the
    /// job used no CPU and wrote nothing. It held its slot the whole time.
    pub parked_us: u64,
    /// What the worker's ffmpeg said about the job. All optional.
    pub stats: crate::state::Stats,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub path: String,
    pub records: usize,
    /// Sorted when read, so percentiles are a lookup.
    pub dispatch_us: Vec<u64>,
    pub duration_us: Vec<u64>,
    /// Speed in thousandths of realtime, sorted. The slow jobs matter: a 0.8x
    /// transcode cannot feed a viewer in realtime.
    pub speed: Vec<u64>,
    /// Frames per second times ten, sorted.
    pub fps: Vec<u64>,
    /// Jobs per content label, for example `1080p h264` or `2160p hevc hdr`.
    pub content: BTreeMap<String, usize>,
    /// Jobs per stream and verdict, for example `video encode` or `audio copy`.
    pub work: BTreeMap<String, usize>,
    pub hosts: BTreeMap<String, HostTally>,
    pub codes: BTreeMap<i32, usize>,
    /// Jobs per class, so a reader sees how much of the load is a probe or a
    /// thumbnail instead of a transcode.
    pub classes: BTreeMap<String, usize>,
    pub jobs: Vec<Job>,
    pub open: usize,
    /// Reap events that matched a start.
    pub reaped: usize,
    /// Events whose start line is missing, so they cannot be attached to a job.
    pub orphan_finish: usize,
    pub orphan_reaped: usize,
    /// The cap each host's own journal gives each content class, with the
    /// sample count and the p05 behind it. Derived from the same tail a
    /// dispatcher reads, never from a stored number (ADR 0033).
    pub sizes: BTreeMap<String, Vec<crate::sizing::Size>>,
}

impl Summary {
    pub fn of(dir: &Path) -> Result<Self> {
        let records = records(dir)?;
        Ok(Self::from_records(dir.display().to_string(), &records))
    }

    pub fn from_records(path: String, records: &[crate::state::Record]) -> Self {
        let mut summary = Self {
            path,
            records: records.len(),
            ..Self::default()
        };
        let mut index: BTreeMap<String, usize> = BTreeMap::new();
        // Jobs parked right now, with the timestamp they parked at.
        let mut parked_since: BTreeMap<String, u64> = BTreeMap::new();

        for record in records {
            match &record.ev {
                Event::Started {
                    job,
                    host,
                    elapsed_us,
                    class,
                    job_kind,
                    input,
                    ..
                } => {
                    let slot = summary.jobs.len();
                    index.insert(job.clone(), slot);
                    summary.jobs.push(Job {
                        id: job.clone(),
                        host: host.clone(),
                        dispatch_us: *elapsed_us,
                        started_ts: record.ts,
                        duration_us: None,
                        code: None,
                        class: unknown_if_empty(class),
                        kind: unknown_if_empty(job_kind),
                        input: input.clone(),
                        reaped: false,
                        parked_us: 0,
                        stats: crate::state::Stats::default(),
                    });
                    let tally = summary.hosts.entry(host.clone()).or_default();
                    tally.jobs += 1;
                    *summary.classes.entry(unknown_if_empty(class)).or_insert(0) += 1;
                }
                Event::Finished {
                    job, code, stats, ..
                } => {
                    let Some(&slot) = index.get(job) else {
                        summary.orphan_finish += 1;
                        continue;
                    };
                    // A job that ends while parked spent its last interval
                    // parked too.
                    if let Some(since) = parked_since.remove(job) {
                        summary.jobs[slot].parked_us += record.ts.saturating_sub(since);
                    }
                    let started = summary.jobs[slot].started_ts;
                    let job = &mut summary.jobs[slot];
                    job.code = Some(*code);
                    job.stats = (**stats).clone();
                    job.duration_us = Some(record.ts.saturating_sub(started));
                    if *code != 0 {
                        let host = job.host.clone();
                        summary.hosts.entry(host).or_default().failed += 1;
                    }
                    *summary.codes.entry(*code).or_insert(0) += 1;
                }
                Event::Reaped { job } => {
                    if let Some(&slot) = index.get(job) {
                        summary.reaped += 1;
                        summary.jobs[slot].reaped = true;
                    } else {
                        summary.orphan_reaped += 1;
                    }
                }
                Event::Parked { job } => {
                    parked_since.entry(job.clone()).or_insert(record.ts);
                }
                Event::Resumed { job } => {
                    if let (Some(&slot), Some(since)) = (index.get(job), parked_since.remove(job)) {
                        summary.jobs[slot].parked_us += record.ts.saturating_sub(since);
                    }
                }
                // An audit line for a startup, not a job.
                Event::Sized { .. } => {}
                Event::HostDown { host } => {
                    summary.hosts.entry(host.clone()).or_default().down += 1;
                }
                Event::HostUp { host } => {
                    summary.hosts.entry(host.clone()).or_default().up += 1;
                }
            }
        }

        summary.open = summary
            .jobs
            .iter()
            .filter(|job| job.code.is_none() && !job.reaped)
            .count();
        summary.dispatch_us = summary.jobs.iter().map(|job| job.dispatch_us).collect();
        summary.dispatch_us.sort_unstable();
        summary.duration_us = summary
            .jobs
            .iter()
            .filter_map(|job| job.duration_us)
            .collect();
        summary.duration_us.sort_unstable();
        for job in &summary.jobs {
            if let Some(speed) = job.stats.speed {
                summary.speed.push((speed * 1000.0).round().max(0.0) as u64);
            }
            if let Some(fps) = job.stats.fps.filter(|fps| *fps > 0.0) {
                // A zero means ffmpeg had not emitted a frame yet; counting it
                // would drag the median down for a reason of no interest.
                summary.fps.push((fps * 10.0).round() as u64);
            }
            if let Some(label) = content_label(&job.stats) {
                *summary.content.entry(label).or_insert(0) += 1;
            }
            for (name, verdict) in [
                ("video", &job.stats.video),
                ("audio", &job.stats.audio),
                ("subtitle", &job.stats.subtitle),
            ] {
                if let Some(verdict) = verdict {
                    *summary.work.entry(format!("{name} {verdict}")).or_insert(0) += 1;
                }
            }
        }
        summary.speed.sort_unstable();
        summary.fps.sort_unstable();
        summary.sizes = sizes(records);
        summary
    }

    pub fn report(&self) -> String {
        let mut out = format!("journal {}\n", self.path);
        out.push_str(&format!(
            "records {}  jobs {}  open {}  reaped {}  orphan_finish {}  orphan_reaped {}\n",
            self.records,
            self.jobs.len(),
            self.open,
            self.reaped,
            self.orphan_finish,
            self.orphan_reaped,
        ));
        out.push_str(&self.line("dispatch", &self.dispatch_us));
        out.push_str(&self.line("duration", &self.duration_us));
        if !self.speed.is_empty() {
            out.push_str(&format!(
                "speed     n={:<5} p50 {:<6} p05 {:<6} min {}\n",
                self.speed.len(),
                speed(percentile(&self.speed, 0.50)),
                speed(percentile(&self.speed, 0.05)),
                speed(self.speed[0]),
            ));
        }
        if !self.fps.is_empty() {
            out.push_str(&format!(
                "fps       n={:<5} p50 {:.1}\n",
                self.fps.len(),
                percentile(&self.fps, 0.50) as f64 / 10.0,
            ));
        }
        if !self.content.is_empty() {
            let labels: Vec<String> = self
                .content
                .iter()
                .map(|(label, n)| format!("{label} {n}"))
                .collect();
            out.push_str(&format!("content   {}\n", labels.join("  ")));
        }
        if !self.work.is_empty() {
            let labels: Vec<String> = self
                .work
                .iter()
                .map(|(label, n)| format!("{label} {n}"))
                .collect();
            out.push_str(&format!("work      {}\n", labels.join("  ")));
        }
        if !self.classes.is_empty() {
            let classes: Vec<String> = self
                .classes
                .iter()
                .map(|(name, n)| format!("{name} {n}"))
                .collect();
            out.push_str(&format!("class     {}\n", classes.join("  ")));
        }
        if !self.codes.is_empty() {
            let codes: Vec<String> = self
                .codes
                .iter()
                .map(|(code, n)| format!("{code}:{n}"))
                .collect();
            out.push_str(&format!("exit codes  {}\n", codes.join(" ")));
        }
        for (host, tally) in &self.hosts {
            out.push_str(&format!(
                "host {host}  jobs {}  failed {}  down {}  up {}\n",
                tally.jobs, tally.failed, tally.down, tally.up
            ));
        }
        for (host, sizes) in &self.sizes {
            for size in sizes {
                // The number the next dispatch will use, and the measure it
                // came from: a cap of 1 with a p05 of 1.2x says the host has
                // 20% of headroom, not that nobody measured it.
                out.push_str(&format!(
                    "size      host {host}  class {:<24} cap {}  n {:<5} p05 {}\n",
                    size.key
                        .as_ref()
                        .map_or_else(|| "heavy".to_string(), |key| key.label()),
                    size.cap,
                    size.samples,
                    match size.p05 {
                        Some(p05) => format!("{p05:.2}x"),
                        None => "none".to_string(),
                    }
                ));
            }
        }
        out.push_str(&self.parked());
        out.push_str(&self.breakdown());
        out
    }

    /// Parked time per job, and the total: a paused job holds a slot and feeds
    /// nobody, so a rising total is what a cap has to discount.
    fn parked(&self) -> String {
        let mut count = 0;
        let mut total = 0;
        let mut longest = 0;
        for job in &self.jobs {
            if job.parked_us > 0 {
                count += 1;
                total += job.parked_us;
                longest = longest.max(job.parked_us);
            }
        }
        if count == 0 {
            return "parked    none\n".to_string();
        }
        format!(
            "parked    {count} of {} jobs, {} total, longest {}\n",
            self.jobs.len(),
            human(total),
            human(longest)
        )
    }

    /// Three groups of rows: what the job does, what it cost, and the light
    /// jobs that waited anyway. A blank line sets them apart from the totals.
    fn breakdown(&self) -> String {
        let mut out = String::from("\n");
        for (name, group) in self.grouped(kind_of) {
            out.push_str(&group.line("kind", &name));
        }
        for (name, group) in self.grouped(class_of) {
            out.push_str(&group.line("class", &name));
        }
        out.push_str(&self.light_wait());
        out
    }

    /// One group per distinct name, sorted. `key` picks the field to split on.
    fn grouped(&self, key: fn(&Job) -> &str) -> Vec<(String, Group)> {
        let mut names: Vec<&str> = self.jobs.iter().map(key).collect();
        names.sort_unstable();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let group = Group::of(self.jobs.iter().filter(|job| key(job) == name));
                (name.to_string(), group)
            })
            .collect()
    }

    /// A Light job that waited over a second means placement or the pool is
    /// wrong: a probe must never queue behind a transcode (ADR 0032).
    fn light_wait(&self) -> String {
        let light = self
            .jobs
            .iter()
            .filter(|job| job.class == crate::Class::Light.name())
            .count();
        if light == 0 {
            return "light wait  none\n".to_string();
        }
        let (waited, slowest) = self.light_waits();
        let slowest = if waited == 0 {
            "-".to_string()
        } else {
            human(slowest)
        };
        format!("light wait  {waited} of {light} light jobs waited over 1 s, slowest {slowest}\n")
    }

    /// Light jobs that waited over a second, and the longest of those. The
    /// count must stay at zero, so a dashboard can alert on it.
    fn light_waits(&self) -> (usize, u64) {
        const LIMIT_US: u64 = 1_000_000;
        let mut waited = 0;
        let mut slowest = 0;
        for job in &self.jobs {
            if job.class == crate::Class::Light.name() && job.dispatch_us > LIMIT_US {
                waited += 1;
                slowest = slowest.max(job.dispatch_us);
            }
        }
        (waited, slowest)
    }

    fn groups_json(&self, key: fn(&Job) -> &str) -> serde_json::Value {
        let mut rows = serde_json::Map::new();
        for (name, group) in self.grouped(key) {
            rows.insert(name, group.json());
        }
        serde_json::Value::Object(rows)
    }

    fn line(&self, label: &str, samples: &[u64]) -> String {
        if samples.is_empty() {
            return format!("{label:<9} none\n");
        }
        format!(
            "{label:<9} n={:<5} p50 {:<9} p95 {:<9} p99 {:<9} max {}\n",
            samples.len(),
            human(percentile(samples, 0.50)),
            human(percentile(samples, 0.95)),
            human(percentile(samples, 0.99)),
            human(percentile(samples, 1.0)),
        )
    }

    pub fn json(&self) -> String {
        let value = serde_json::json!({
            "journal": self.path,
            "records": self.records,
            "jobs": self.jobs.len(),
            "open": self.open,
            "reaped": self.reaped,
            "orphan_finish": self.orphan_finish,
            "orphan_reaped": self.orphan_reaped,
            "dispatch_us": stats(&self.dispatch_us),
            "duration_us": stats(&self.duration_us),
            "speed_x1000": stats(&self.speed),
            "fps_x10": stats(&self.fps),
            "content": self.content,
            "work": self.work,
            "class": self.classes,
            "kinds": self.groups_json(kind_of),
            "classes": self.groups_json(class_of),
            "light_wait_over_1s": self.light_waits().0,
            "sizes": self.sizes_json(),
            "parked_us": self.parked_json(),
        });
        value.to_string()
    }

    /// One row per host and class: the cap the next dispatch will use, the
    /// sample count and the p05 behind it.
    fn sizes_json(&self) -> serde_json::Value {
        let mut hosts = serde_json::Map::new();
        for (host, sizes) in &self.sizes {
            let rows: Vec<serde_json::Value> = sizes
                .iter()
                .map(|size| {
                    serde_json::json!({
                        "class": size.key.as_ref().map_or_else(|| "heavy".to_string(), |key| key.label()),
                        "cap": size.cap,
                        "samples": size.samples,
                        "p05": size.p05,
                    })
                })
                .collect();
            hosts.insert(host.clone(), serde_json::Value::Array(rows));
        }
        serde_json::Value::Object(hosts)
    }

    /// Parked microseconds per job, so a dashboard reads the pause of one
    /// stream instead of a total nobody can attribute.
    fn parked_json(&self) -> serde_json::Value {
        let mut rows = serde_json::Map::new();
        for job in &self.jobs {
            if job.parked_us > 0 {
                rows.insert(job.id.clone(), serde_json::Value::from(job.parked_us));
            }
        }
        serde_json::Value::Object(rows)
    }
}

/// The cap a host's journal gives each content class. Same rules the
/// dispatcher applies at startup, on the same tail, so the report and the
/// placement cannot disagree.
fn sizes(records: &[crate::state::Record]) -> BTreeMap<String, Vec<crate::sizing::Size>> {
    let cpus = crate::config::cpus();
    crate::sizing::samples(records)
        .iter()
        .map(|(host, samples)| {
            let mut sizes: Vec<crate::sizing::Size> = crate::sizing::classes(samples)
                .iter()
                .filter_map(|key| crate::sizing::size_class(samples, key, cpus))
                .collect();
            if sizes.is_empty() {
                sizes.push(crate::sizing::Size::none());
            }
            (host.clone(), sizes)
        })
        .collect()
}

/// Percentiles for a set of jobs: one row of the breakdown, and one JSON
/// object.
#[derive(Debug, Default, Clone)]
struct Group {
    n: usize,
    dispatch_p50: u64,
    dispatch_p95: u64,
    duration_p50: Option<u64>,
    duration_p95: Option<u64>,
    speed_p05: Option<u64>,
    speed_p50: Option<u64>,
}

impl Group {
    fn of<'a>(jobs: impl IntoIterator<Item = &'a Job>) -> Group {
        let mut dispatch = Vec::new();
        let mut duration = Vec::new();
        let mut speed = Vec::new();
        let mut n = 0;
        for job in jobs {
            n += 1;
            dispatch.push(job.dispatch_us);
            if let Some(us) = job.duration_us {
                duration.push(us);
            }
            if let Some(x) = job.stats.speed {
                speed.push((x * 1000.0).round().max(0.0) as u64);
            }
        }
        dispatch.sort_unstable();
        duration.sort_unstable();
        speed.sort_unstable();
        let rank = |sorted: &[u64], fraction: f64| -> Option<u64> {
            if sorted.is_empty() {
                None
            } else {
                Some(percentile(sorted, fraction))
            }
        };
        Group {
            n,
            dispatch_p50: percentile(&dispatch, 0.50),
            dispatch_p95: percentile(&dispatch, 0.95),
            duration_p50: rank(&duration, 0.50),
            duration_p95: rank(&duration, 0.95),
            speed_p05: rank(&speed, 0.05),
            speed_p50: rank(&speed, 0.50),
        }
    }

    fn line(&self, label: &str, name: &str) -> String {
        format!(
            "{label:<9} {name:<10} n={:<5} dispatch p50 {:<9} p95 {:<9} duration p50 {:<9} p95 {:<9} speed p05 {:<7} p50 {}\n",
            self.n,
            human(self.dispatch_p50),
            human(self.dispatch_p95),
            dash(self.duration_p50.map(human)),
            dash(self.duration_p95.map(human)),
            dash(self.speed_p05.map(speed)),
            dash(self.speed_p50.map(speed)),
        )
    }

    /// A percentile with no sample is null, not zero: a zero would read as a
    /// real measurement of no time.
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "n": self.n,
            "dispatch_p50_us": self.dispatch_p50,
            "dispatch_p95_us": self.dispatch_p95,
            "duration_p50_us": self.duration_p50,
            "duration_p95_us": self.duration_p95,
            "speed_p05": self.speed_p05,
            "speed_p50": self.speed_p50,
        })
    }
}

fn kind_of(job: &Job) -> &str {
    &job.kind
}

fn class_of(job: &Job) -> &str {
    &job.class
}

fn dash(value: Option<String>) -> String {
    value.unwrap_or_else(|| "-".to_string())
}

fn unknown_if_empty(name: &str) -> String {
    if name.is_empty() {
        "unknown".to_string()
    } else {
        name.to_string()
    }
}

/// One content label per job, the way a dashboard slices it: the source size,
/// its codec, and HDR when the transfer function says so.
fn content_label(stats: &crate::state::Stats) -> Option<String> {
    let height = stats.src_h?;
    let codec = stats.src_codec.as_deref()?;
    let mut label = format!("{height}p {codec}");
    if let Some(hdr) = &stats.src_hdr {
        label.push(' ');
        label.push_str(hdr);
    }
    Some(label)
}

/// Thousandths of realtime as `1.24x`.
fn speed(permille: u64) -> String {
    format!("{:.2}x", permille as f64 / 1000.0)
}

fn stats(samples: &[u64]) -> serde_json::Value {
    serde_json::json!({
        "n": samples.len(),
        "p50": percentile(samples, 0.50),
        "p95": percentile(samples, 0.95),
        "p99": percentile(samples, 0.99),
        "max": percentile(samples, 1.0),
    })
}

/// Microseconds in the unit a reader thinks in. Two digits are enough at every
/// scale: nobody needs 12.4 s shown to the microsecond.
pub fn human(us: u64) -> String {
    match us {
        0..=999 => format!("{us} us"),
        1_000..=999_999 => format!("{:.2} ms", us as f64 / 1_000.0),
        1_000_000..=59_999_999 => format!("{:.1} s", us as f64 / 1_000_000.0),
        _ => format!("{:.2} min", us as f64 / 60_000_000.0),
    }
}

/// Load the config and print the summary, or one JSON object.
pub fn main(args: &[String]) -> Result<i32> {
    let mut dir = None;
    let mut json = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--dir" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    return Err(Error::Config("--dir needs a path".into()));
                };
                dir = Some(std::path::PathBuf::from(value));
            }
            "-h" | "--help" => {
                eprintln!("usage: rrffmpeg-stats [--dir STATE_DIR] [--json]");
                return Ok(0);
            }
            other => {
                return Err(Error::Config(format!("unknown option {other}")));
            }
        }
        i += 1;
    }
    let dir = match dir {
        Some(dir) => dir,
        None => crate::Config::load()?.state_dir,
    };
    let summary = Summary::of(&dir)?;
    if json {
        println!("{}", summary.json());
    } else {
        print!("{}", summary.report());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let samples = [1, 2, 3, 4];
        assert_eq!(percentile(&samples, 0.50), 2);
        assert_eq!(percentile(&samples, 0.95), 4);
        assert_eq!(percentile(&samples, 1.0), 4);
        assert_eq!(percentile(&[], 0.5), 0);
    }

    #[test]
    fn human_units() {
        assert_eq!(human(412), "412 us");
        assert_eq!(human(1_500), "1.50 ms");
        assert_eq!(human(12_400_000), "12.4 s");
        assert_eq!(human(120_000_000), "2.00 min");
    }

    #[test]
    fn percentile_of_an_empty_slice_returns_zero() {
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[], 1.0), 0);
    }

    #[test]
    fn percentile_above_one_is_clamped() {
        let samples = [1, 2, 3, 4];
        assert_eq!(percentile(&samples, 1.5), 4);
        assert_eq!(percentile(&samples, 9.0), 4);
    }

    #[test]
    fn human_rounds_at_the_unit_boundary() {
        // 999_999 us stays in the millisecond branch, so the two-decimal
        // rounding yields "1000.00 ms". It never crosses into a "1000.0 s"
        // label, which is the boundary this guards.
        assert_eq!(human(999_999), "1000.00 ms");
        assert_eq!(human(1_000_000), "1.0 s");
    }

    #[test]
    fn a_started_and_a_finished_make_one_measured_job() {
        let path = "journal".to_string();
        let records = vec![
            record(1, 1_000, started("a", "worker", 400)),
            record(2, 1_300_000, finished("a", 0)),
            record(3, 2_000, started("b", "worker", 900)),
        ];
        let summary = Summary::from_records(path, &records);
        assert_eq!(summary.jobs.len(), 2);
        assert_eq!(summary.open, 1);
        assert_eq!(summary.duration_us, vec![1_299_000]);
        assert_eq!(summary.codes.get(&0), Some(&1));
        assert_eq!(summary.hosts["worker"].jobs, 2);
        let report = summary.report();
        assert!(report.contains("dispatch"), "{report}");
        assert!(report.contains("host worker"), "{report}");
        assert!(report.contains("class     heavy 2"), "{report}");
    }

    #[test]
    fn a_failed_job_is_counted_on_its_host() {
        let records = vec![
            record(1, 0, started("a", "worker", 500)),
            record(2, 10, finished("a", 1)),
        ];
        let summary = Summary::from_records("journal".into(), &records);
        assert_eq!(summary.hosts["worker"].failed, 1);
        assert!(summary.json().contains("\"p99\":500"), "{}", summary.json());
    }

    #[test]
    fn a_finished_without_a_started_is_counted_as_an_orphan() {
        let records = vec![
            record(1, 0, started("a", "worker", 100)),
            record(2, 10, finished("ghost", 0)),
            record(3, 20, finished("a", 0)),
        ];
        let summary = Summary::from_records("journal".into(), &records);
        assert_eq!(summary.orphan_finish, 1);
        assert_eq!(summary.jobs.len(), 1);
        assert_eq!(summary.open, 0);
        assert!(
            summary.report().contains("orphan_finish 1"),
            "{}",
            summary.report()
        );
        assert!(summary.json().contains("\"orphan_finish\":1"));
    }

    #[test]
    fn a_reaped_without_a_started_is_counted_as_an_orphan() {
        let records = vec![
            record(1, 0, started("a", "worker", 100)),
            record(
                2,
                10,
                Event::Reaped {
                    job: "ghost".into(),
                },
            ),
            record(3, 20, Event::Reaped { job: "a".into() }),
        ];
        let summary = Summary::from_records("journal".into(), &records);
        assert_eq!(summary.orphan_reaped, 1);
        assert_eq!(summary.reaped, 1);
        assert_eq!(summary.open, 0);
        assert!(summary.report().contains("orphan_reaped 1"));
    }

    fn record(seq: u64, ts: u64, ev: Event) -> crate::state::Record {
        crate::state::Record { seq, ts, ev }
    }

    fn started(job: &str, host: &str, elapsed_us: u64) -> Event {
        job_started(job, host, elapsed_us, "heavy", "transcode")
    }

    fn job_started(job: &str, host: &str, elapsed_us: u64, class: &str, kind: &str) -> Event {
        Event::Started {
            job: job.into(),
            host: host.into(),
            elapsed_us,
            program: "ffmpeg".into(),
            in_flight: 1,
            class: class.into(),
            pool: class.into(),
            job_kind: kind.into(),
            input: "/media/ep.mkv".into(),
        }
    }

    fn finished(job: &str, code: i32) -> Event {
        Event::Finished {
            job: job.into(),
            code,
            host: "worker".into(),
            dur_us: 0,
            stats: Box::default(),
        }
    }

    #[test]
    fn a_job_with_a_sample_reports_its_speed_and_content() {
        let stats = crate::state::Stats {
            fps: Some(23.98),
            speed: Some(1.24),
            src_w: Some(1920),
            src_h: Some(1080),
            src_codec: Some("hevc".into()),
            src_hdr: None,
            dst_w: Some(1280),
            dst_h: Some(720),
            dst_codec: Some("h264".into()),
            video: Some("encode".into()),
            audio: Some("copy".into()),
            subtitle: None,
        };
        let records = vec![
            record(1, 0, started("a", "worker", 500)),
            record(
                2,
                10,
                Event::Finished {
                    job: "a".into(),
                    code: 0,
                    host: "worker".into(),
                    dur_us: 10,
                    stats: Box::new(stats),
                },
            ),
        ];
        let summary = Summary::from_records("journal".into(), &records);
        assert_eq!(summary.speed, vec![1240]);
        assert_eq!(summary.fps, vec![240]);
        assert_eq!(summary.content.get("1080p hevc"), Some(&1));
        assert_eq!(summary.work.get("video encode"), Some(&1));
        assert_eq!(summary.work.get("audio copy"), Some(&1));
        let report = summary.report();
        assert!(report.contains("1.24x"), "{report}");
        assert!(report.contains("1080p hevc"), "{report}");
        assert!(report.contains("video encode"), "{report}");
        assert!(
            summary.json().contains("\"p50\":1240"),
            "{}",
            summary.json()
        );
    }

    /// One row per kind, one per class, and the light job that waited anyway.
    #[test]
    fn the_breakdown_splits_by_kind_and_class() {
        let records = vec![
            record(1, 0, job_started("t", "worker", 100, "light", "trickplay")),
            record(
                2,
                10,
                job_started("th", "worker", 2_000_000, "light", "thumbnail"),
            ),
            record(3, 20, job_started("x", "worker", 300, "heavy", "transcode")),
        ];
        let summary = Summary::from_records("journal".into(), &records);
        let report = summary.report();
        for kind in ["trickplay", "thumbnail", "transcode"] {
            assert!(report.contains(&format!("kind      {kind}")), "{report}");
        }
        assert!(report.contains("class     light      n=2"), "{report}");
        assert!(report.contains("class     heavy      n=1"), "{report}");
        assert!(
            report.contains("light wait  1 of 2 light jobs waited over 1 s, slowest 2.0 s"),
            "{report}"
        );

        let json: serde_json::Value = serde_json::from_str(&summary.json()).expect("json");
        assert_eq!(json["kinds"]["trickplay"]["n"], 1, "{json}");
        assert_eq!(json["classes"]["light"]["n"], 2, "{json}");
        assert_eq!(json["light_wait_over_1s"], 1, "{json}");
        assert!(
            json["kinds"]["transcode"]["duration_p50_us"].is_null(),
            "{json}"
        );
    }
}
