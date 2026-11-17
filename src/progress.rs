//! Keep the numbers ffmpeg prints on its own stderr.
//!
//! `rrffmpeg` is one process per job, so there is no daemon to hold state and a
//! static slot is enough: the transports feed every stderr chunk here as it
//! passes, and `main` takes the sample once the job is over. Nothing is
//! computed on the dispatch path.
//!
//! Two kinds of line carry what we want, and nothing else does:
//!
//! - a `Stream #0:0: Video: ...` line, under `Input #0` or `Output #0`, gives
//!   the codec, the frame size and the transfer function (HDR);
//! - a `Stream #0:0 -> #0:0 (copy)` line under `Stream mapping:` says whether
//!   the stream is copied or re-encoded, which is how a direct stream and a
//!   transcode are told apart;
//! - a stats line, rewritten in place with `\r`, gives `fps`, `bitrate` and
//!   `speed`.
//!
//! A job run with `-loglevel error` or `-nostats` prints neither, so it records
//! no sample. That is a gap in the data, never a failure.

use crate::state::Stats;
use std::sync::{Mutex, OnceLock, PoisonError};

/// Longest run of bytes kept without a line break. A progress line is about
/// 100 bytes; anything past this is an error message we do not parse.
const PARTIAL_CAP: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Side {
    #[default]
    Unknown,
    Input,
    Output,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    Audio,
    Subtitle,
    Other,
}

#[derive(Default)]
struct Tap {
    partial: Vec<u8>,
    side: Side,
    stats: Stats,
    /// Input stream index to kind, so a `Stream mapping:` line can name what
    /// it copies or re-encodes.
    inputs: Vec<(u32, Kind)>,
    /// Bytes of the partial buffer were dropped, so its head is not a line
    /// start. Skip to the next break instead of parsing half a line.
    clipped: bool,
}

impl Tap {
    fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\r' || byte == b'\n' {
                if !self.partial.is_empty() {
                    let line = String::from_utf8_lossy(&self.partial).into_owned();
                    self.partial.clear();
                    if !self.clipped {
                        self.line(&line);
                    }
                }
                // A break always resyncs, even when the buffer was already
                // emptied by the cap.
                self.clipped = false;
                continue;
            }
            self.partial.push(byte);
            if self.partial.len() > PARTIAL_CAP {
                self.partial.clear();
                self.clipped = true;
            }
        }
    }

    fn line(&mut self, line: &str) {
        // The section marker decides whether the `Stream #0:0` lines that
        // follow describe the source or the output.
        if line.contains("Output #0") {
            self.side = Side::Output;
            return;
        }
        if line.contains("Input #0") {
            self.side = Side::Input;
            return;
        }
        if line.contains(" -> ") && after_index(line).is_some() {
            self.mapping(line);
            return;
        }
        match stream_head(line) {
            Some((index, kind, rest)) => self.stream(index, kind, rest),
            None if line.contains("fps=") => self.counters(line),
            None => {}
        }
    }

    fn stream(&mut self, index: u32, kind: Kind, line: &str) {
        if self.side == Side::Input {
            if self.inputs.len() < 16 {
                self.inputs.push((index, kind));
            }
            // The first input video stream is the one being transcoded.
            if kind == Kind::Video && self.stats.src_w.is_none() {
                self.stats.src_codec = codec(line);
                self.stats.src_hdr = hdr(line).map(str::to_string);
                if let Some((w, h)) = resolution(line) {
                    self.stats.src_w = Some(w);
                    self.stats.src_h = Some(h);
                }
            }
        } else if self.side == Side::Output && kind == Kind::Video && self.stats.dst_w.is_none() {
            self.stats.dst_codec = codec(line);
            if let Some((w, h)) = resolution(line) {
                self.stats.dst_w = Some(w);
                self.stats.dst_h = Some(h);
            }
        }
    }

    /// `Stream #0:0 -> #0:0 (copy)` copies, `(hevc ... -> h264 ...)`
    /// re-encodes. A stream that is neither copied nor converted by ffmpeg
    /// does not exist in the output, so the mapping is the whole truth.
    fn mapping(&mut self, line: &str) {
        let Some((index, _)) = after_index(line) else {
            return;
        };
        let Some(&(_, kind)) = self.inputs.iter().find(|(i, _)| *i == index) else {
            return;
        };
        let verdict = if line.contains("(copy)") {
            "copy"
        } else {
            "encode"
        };
        let slot = match kind {
            Kind::Video => &mut self.stats.video,
            Kind::Audio => &mut self.stats.audio,
            Kind::Subtitle => &mut self.stats.subtitle,
            Kind::Other => return,
        };
        *slot = Some(verdict.to_string());
    }

    /// `fps` and `speed` are running averages since the job started, so the
    /// last line is not the throughput: startup drags it down, and a stall or
    /// a stream pause leaves the frame count behind while the clock runs. Keep
    /// the highest average reached, which is what the player measured live.
    fn counters(&mut self, line: &str) {
        if let Some(fps) = number_after(line, "fps=") {
            self.stats.fps = Some(fps.max(self.stats.fps.unwrap_or(0.0)));
        }
        if let Some(speed) = number_after(line, "speed=") {
            self.stats.speed = Some(speed.max(self.stats.speed.unwrap_or(0.0)));
        }
        // The bitrate is a moment, not a running total, so the last one wins.
        if let Some(kbps) = number_after(line, "bitrate=") {
            self.stats.kbps = Some(kbps);
        }
    }
}

/// The number that follows `key`, past the padding ffmpeg puts in front of it
/// (`fps= 85`, `speed=   5x`), or `None` when it prints `N/A`.
fn number_after(line: &str, key: &str) -> Option<f64> {
    let rest = line.split_once(key)?.1.trim_start_matches(' ');
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// `Stream #0:2[0x3](und): Subtitle: ...` gives the input index and the kind.
fn stream_head(line: &str) -> Option<(u32, Kind, &str)> {
    let (index, rest) = after_index(line)?;
    let kind = if rest.contains(": Video:") {
        Kind::Video
    } else if rest.contains(": Audio:") {
        Kind::Audio
    } else if rest.contains(": Subtitle:") {
        Kind::Subtitle
    } else if rest.contains(": Data:") {
        Kind::Other
    } else {
        return None;
    };
    Some((index, kind, rest))
}

/// Index of the stream a line describes, plus the rest of the line.
fn after_index(line: &str) -> Option<(u32, &str)> {
    let rest = line.split("Stream #0:").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((digits.parse().ok()?, rest))
}

/// `Video: h264 (High) (avc1 ...)` gives `h264`.
fn codec(line: &str) -> Option<String> {
    line.split(" Video: ")
        .nth(1)
        .and_then(|rest| rest.split([' ', ',', '(']).next())
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

fn hdr(line: &str) -> Option<&'static str> {
    if line.contains("smpte2084") {
        Some("hdr10")
    } else if line.contains("arib-std-b67") {
        Some("hlg")
    } else {
        None
    }
}

/// First `NUMBERxNUMBER` token in a stream description.
fn resolution(text: &str) -> Option<(u32, u32)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // A width starts a token: this skips the `0x` of a stream id such as
        // `Stream #0:0[0x1]`.
        if bytes[i].is_ascii_digit() && (i == 0 || bytes[i - 1] == b' ') {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if bytes.get(i) == Some(&b'x') {
                let height_start = i + 1;
                let mut end = height_start;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end > height_start {
                    let width: u32 = text[start..i].parse().ok()?;
                    let height: u32 = text[height_start..end].parse().ok()?;
                    // A hex id such as `0x31637661` reads as a size otherwise.
                    if width >= 16 && height >= 16 {
                        return Some((width, height));
                    }
                }
            }
            continue;
        }
        i += 1;
    }
    None
}

fn tap() -> &'static Mutex<Tap> {
    static TAP: OnceLock<Mutex<Tap>> = OnceLock::new();
    TAP.get_or_init(|| Mutex::new(Tap::default()))
}

/// Read one chunk of a job's stderr. Safe to call from any thread, in any
/// order: a line split across two chunks is joined by the partial buffer.
pub fn feed(chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    tap()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .feed(chunk);
}

/// Take what the job printed and reset, so a retry starts clean.
pub fn take() -> Stats {
    let mut guard = tap().lock().unwrap_or_else(PoisonError::into_inner);
    std::mem::take(&mut guard.stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(chunks: &[&str]) -> Stats {
        let mut tap = Tap::default();
        for chunk in chunks {
            tap.feed(chunk.as_bytes());
        }
        // A last line with no break is a line cut short by the exit: drop it.
        tap.stats
    }

    const INPUT: &str = "Input #0, matroska,webm, from '/m/x.mkv':\r\n  \
        Stream #0:0: Video: hevc (Main 10), yuv420p10le(tv, bt2020nc/bt2020/smpte2084), \
        3840x2160 [SAR 1:1 DAR 16:9], 23.98 fps\n";
    const OUTPUT: &str = "Output #0, hls, to '/dev/shm/x.m3u8':\n  \
        Stream #0:0: Video: h264 (High), vaapi(tv, bt709, progressive), 1920x1080, \
        4000 kb/s, 23.98 fps\n";
    const STATS: &str = "frame=  100 fps= 24 q=-0.0 size=  512kB time=00:00:04.17 \
        bitrate=1006.2kbits/s speed=1.02x";

    #[test]
    fn a_source_and_an_output_give_the_content_shape() {
        let stats = parsed(&[INPUT, OUTPUT]);
        assert_eq!(stats.src_codec.as_deref(), Some("hevc"));
        assert_eq!((stats.src_w, stats.src_h), (Some(3840), Some(2160)));
        assert_eq!(stats.src_hdr.as_deref(), Some("hdr10"));
        assert_eq!(stats.dst_codec.as_deref(), Some("h264"));
        assert_eq!((stats.dst_w, stats.dst_h), (Some(1920), Some(1080)));
    }

    #[test]
    fn the_highest_running_average_wins() {
        let stats = parsed(&[
            &format!("{STATS}\r"),
            &format!("{STATS}\r"),
            "frame=2 fps=30 speed=1.40x bitrate=1006.2kbits/s\r",
            // The tail of a job whose frame count stopped moving while the
            // clock kept running, as a throttled or stalled stream does.
            "frame=2 fps=12 q=-0.0 time=00:00:09.00 bitrate=N/A speed=0.5x\r",
        ]);
        assert_eq!(stats.fps, Some(30.0));
        assert_eq!(stats.speed, Some(1.40));
        assert_eq!(stats.kbps, Some(1006.2));
    }

    #[test]
    fn the_padding_ffmpeg_puts_before_a_counter_does_not_hide_it() {
        let stats = parsed(&[
            "frame=   43 fps=0.0 q=-0.0 size=N/A time=00:00:00.00 bitrate=N/A speed=   0x\r",
            "frame= 1491 fps=123 q=-0.0 size=N/A time=00:01:00.39 bitrate=N/A speed=   5x\r",
            "frame= 1725 fps= 86 q=-0.0 Lsize=N/A time=00:01:10.14 bitrate=N/A speed= 3.5x\r",
        ]);
        assert_eq!(stats.fps, Some(123.0));
        assert_eq!(stats.speed, Some(5.0));
        assert_eq!(stats.kbps, None);
    }

    #[test]
    fn a_line_split_across_chunks_still_parses() {
        let stats = parsed(&[
            "frame=1 fps=2",
            "3.5 q=0 speed=1.1",
            "0x bitrate=900kbits/s\r",
        ]);
        assert_eq!(stats.fps, Some(23.5));
        assert_eq!(stats.speed, Some(1.10));
    }

    #[test]
    fn an_hdr_transfer_function_is_named() {
        let hdr = |transfer: &str| {
            parsed(&[
                "Input #0, matroska,webm, from 'x':\n",
                &format!("  Stream #0:0: Video: hevc, yuv420p10le({transfer}), 3840x2160\n"),
            ])
            .src_hdr
        };
        assert_eq!(hdr("smpte2084").as_deref(), Some("hdr10"));
        assert_eq!(hdr("arib-std-b67").as_deref(), Some("hlg"));
        assert_eq!(hdr("bt709"), None);
    }

    #[test]
    fn a_mapping_line_says_copy_or_encode() {
        let stats = parsed(&[
            "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'x':\n",
            "  Stream #0:0[0x1](und): Video: h264 (High) (avc1 / 0x31637661), yuv420p(progressive), 320x240 [SAR 1:1 DAR 4:3], 24 fps\n",
            "  Stream #0:1[0x2](und): Audio: aac (LC) (mp4a / 0x6134706D), 44100 Hz, mono\n",
            "  Stream #0:2[0x3](und): Subtitle: mov_text (tx3g / 0x67337874), 0 kb/s\n",
            "Stream mapping:\n",
            "  Stream #0:0 -> #0:0 (copy)\n",
            "  Stream #0:1 -> #0:1 (aac (native) -> aac (native))\n",
            "  Stream #0:2 -> #0:2 (copy)\n",
        ]);
        assert_eq!(stats.video.as_deref(), Some("copy"));
        assert_eq!(stats.audio.as_deref(), Some("encode"));
        assert_eq!(stats.subtitle.as_deref(), Some("copy"));
        assert_eq!(stats.src_codec.as_deref(), Some("h264"));
        assert_eq!((stats.src_w, stats.src_h), (Some(320), Some(240)));
    }

    #[test]
    fn a_job_with_no_mapping_records_no_verdict() {
        let stats = parsed(&[INPUT]);
        assert_eq!(stats.video, None);
        assert_eq!(stats.audio, None);
    }

    #[test]
    fn a_line_that_is_not_a_stream_or_a_counter_changes_nothing() {
        let stats = parsed(&["  libva info: VA-API version 1.20.0\n", "Stream mapping:\n"]);
        assert_eq!(stats.fps, None);
        assert_eq!(stats.src_w, None);
    }

    #[test]
    fn an_overflowing_line_is_dropped_rather_than_parsed_half() {
        let mut tap = Tap::default();
        tap.feed(&vec![b'a'; PARTIAL_CAP + 1]);
        tap.feed(b"\rframe=1 fps=30 speed=2.0x\r");
        assert_eq!(tap.stats.fps, Some(30.0));
    }

    #[test]
    fn taking_resets_the_slot() {
        feed(b"frame=1 fps=24 speed=1.0x\r");
        assert_eq!(take().fps, Some(24.0));
        assert_eq!(take().fps, None);
    }
}
