//! Dispatch core shared by the binary and the tests.

pub mod agent;
pub mod capabilities;
pub mod config;
pub mod janitor;
pub mod jellyfin;
pub mod locks;
pub mod log;
pub mod paths;
pub mod progress;
pub mod queue;
pub mod scheduler;
pub mod sizing;
pub mod state;
pub mod stats;
pub mod transport;

pub use config::{Cap, Config, Host, PathMap, Program};

use std::ffi::{OsStr, OsString};
use std::fmt;

/// What a job costs a worker.
///
/// A Light job reads a handful of frames: a probe, a thumbnail, a trickplay
/// scan. A Heavy job encodes or remuxes a stream and can hold a slot for
/// minutes. The split is what keeps a frame job out of a transcode's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Light,
    Heavy,
}

impl Class {
    /// The name the journal and `rrffmpeg-stats` use.
    pub fn name(self) -> &'static str {
        match self {
            Class::Light => "light",
            Class::Heavy => "heavy",
        }
    }

    /// Classify from the program and the arguments, nothing else.
    ///
    /// Jellyfin sends every call through the same binary, so these are the
    /// only shapes to go on:
    ///
    /// - `ffprobe`: reads metadata and writes nothing, so always Light.
    /// - `-f image2`: the output is a still or a numbered series, not a live
    ///   stream. The trickplay sprite writer (`-c:v mjpeg -f image2`) lands
    ///   here.
    /// - `-frames:v 1`: one frame, the Jellyfin thumbnail.
    /// - `-skip_frame nokey`: key frames only, the Jellyfin trickplay scan.
    ///
    /// Everything else is Heavy: a video encode, a remux, a tonemap transcode.
    pub fn of(program: Program, args: &[OsString]) -> Class {
        if program == Program::Ffprobe || lists_and_exits(args) {
            return Class::Light;
        }
        let stills = value_of(args, "-f") == Some("image2");
        let one_frame = value_of(args, "-frames:v") == Some("1");
        let key_frames = value_of(args, "-skip_frame") == Some("nokey");
        if stills || one_frame || key_frames {
            return Class::Light;
        }
        Class::Heavy
    }
}

/// The argument after `flag`, so `-f image2` is one pair and not two strings
/// matched apart. A flag whose value is missing reads as no value.
fn value_of<'a>(args: &'a [OsString], flag: &str) -> Option<&'a str> {
    let wanted = OsStr::new(flag);
    let at = args.iter().position(|arg| arg.as_os_str() == wanted)?;
    args.get(at + 1)?.to_str()
}

/// The flags that make ffmpeg print a listing and exit.
const PROBE_LISTS: [&str; 5] = ["-version", "-encoders", "-hwaccels", "-filters", "-h"];

/// A startup probe: ffmpeg prints a list and exits, in milliseconds.
///
/// Jellyfin asks for `-version`, `-encoders` and `-hwaccels` when it starts.
/// They take no input, so they must never queue behind a transcode: with an
/// auto-sized cap of 1 that wait is the full `queue_wait_secs`. The `-i`
/// guard keeps a real job that happens to mention `-h` elsewhere out of the
/// rule (ADR 0032).
fn lists_and_exits(args: &[OsString]) -> bool {
    !has_flag(args, "-i") && PROBE_LISTS.iter().any(|flag| has_flag(args, flag))
}

/// One dispatched transcode.
///
/// `id` is unique across restarts, so a row left behind by a killed client
/// can never be mistaken for a live job.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub program: Program,
    pub args: Vec<OsString>,
    /// Lower runs first: probes and seeks beat long transcodes.
    pub priority: i32,
    /// Capabilities the worker must advertise, for example `vaapi` plus
    /// `vaapi_hevc10`. All of them must match, because a host that encodes
    /// H.264 in hardware can still lack 10-bit HEVC.
    pub capabilities: Vec<String>,
}

impl Job {
    /// What this job costs a worker, read from what ffmpeg is asked to do, so
    /// the class cannot drift from the command.
    pub fn class(&self) -> Class {
        Class::of(self.program, &self.args)
    }

    /// What this job does, for `rrffmpeg-stats`.
    pub fn kind(&self) -> Kind {
        Kind::of(self.program, &self.args)
    }

    /// The input path, for `rrffmpeg-stats`. `file:` is the scheme Jellyfin
    /// always sends, and nothing else is stripped.
    pub fn input(&self) -> String {
        value_of(&self.args, "-i")
            .map(|value| value.strip_prefix("file:").unwrap_or(value).to_string())
            .unwrap_or_default()
    }
}

/// What one job does, in the words `rrffmpeg-stats` reports.
///
/// `Class` says what the job costs a worker; `Kind` says what it is. They are
/// separate because an analysis job is a full decode (Heavy) that is not
/// playback, and a sprite is Light while a remux is Heavy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Probe,
    Thumbnail,
    Trickplay,
    Sprite,
    Analysis,
    Audio,
    Transcode,
    Remux,
}

impl Kind {
    /// The name the journal and `rrffmpeg-stats` use.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Probe => "probe",
            Kind::Thumbnail => "thumbnail",
            Kind::Trickplay => "trickplay",
            Kind::Sprite => "sprite",
            Kind::Analysis => "analysis",
            Kind::Audio => "audio",
            Kind::Transcode => "transcode",
            Kind::Remux => "remux",
        }
    }

    /// Classify from the program and the arguments, in this order. The order
    /// is the rule: a thumbnail is also an image2 writer, so it must win over
    /// `Sprite`, and a trickplay scan is caught before its `-f image2`.
    ///
    /// - `ffprobe`: metadata only, `probe`.
    /// - `-frames:v 1`: the Jellyfin thumbnail.
    /// - `-skip_frame nokey`: the Jellyfin trickplay scan.
    /// - `-f image2` without `-skip_frame nokey`: a sprite sheet, `sprite`.
    /// - `-f null`: a full decode thrown away, which is what IntroSkipper's
    ///   black-frame scan does. Heavy, and not playback, so `analysis`.
    /// - `-vn`, or every `-map` naming audio or subtitle streams: `audio`.
    /// - a video stream copied (`-c:v copy`, `-codec:v:0 copy`, `-c copy`):
    ///   `remux`.
    /// - everything else encodes video: `transcode`.
    pub fn of(program: Program, args: &[OsString]) -> Kind {
        if program == Program::Ffprobe || lists_and_exits(args) {
            return Kind::Probe;
        }
        let format = value_of(args, "-f");
        if value_of(args, "-frames:v") == Some("1") {
            return Kind::Thumbnail;
        }
        if value_of(args, "-skip_frame") == Some("nokey") {
            return Kind::Trickplay;
        }
        match format {
            Some("image2") => return Kind::Sprite,
            Some("null") => return Kind::Analysis,
            _ => {}
        }
        if only_audio_output(args) {
            return Kind::Audio;
        }
        if copies_video(args) {
            return Kind::Remux;
        }
        Kind::Transcode
    }
}

/// No video leaves this job: `-vn`, or a `-map` list that names only audio or
/// subtitle streams of a stream group (`0:a`, `0:a:0`, `0:s:0`).
fn only_audio_output(args: &[OsString]) -> bool {
    if has_flag(args, "-vn") {
        return true;
    }
    let mapped = args
        .windows(2)
        .filter(|pair| pair[0] == "-map")
        .filter_map(|pair| pair[1].to_str());
    let mut any = false;
    let mut no_video = true;
    for selector in mapped {
        any = true;
        // `0:a` and `0:s:1` select a group; `0:1` and `0:v` do not prove it.
        let group = selector.split(':').nth(1).unwrap_or_default();
        if !(group.starts_with('a') || group.starts_with('s')) {
            no_video = false;
        }
    }
    any && no_video
}

/// True when a video stream is copied rather than encoded.
fn copies_video(args: &[OsString]) -> bool {
    for pair in args.windows(2) {
        let Some(flag) = pair[0].to_str() else {
            continue;
        };
        let Some(value) = pair[1].to_str() else {
            continue;
        };
        if value != "copy" {
            continue;
        }
        // `-c copy` and `-codec copy` cover every stream, video included.
        // `-c:v`, `-c:v:0`, `-codec:v`, and `-codec:v:0` name the video.
        if flag == "-c"
            || flag == "-codec"
            || flag.starts_with("-c:v")
            || flag.starts_with("-codec:v")
        {
            return true;
        }
    }
    false
}

fn has_flag(args: &[OsString], flag: &str) -> bool {
    let wanted = OsStr::new(flag);
    args.iter().any(|arg| arg.as_os_str() == wanted)
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error, String),
    Config(String),
    State(String),
    Transport(String),
    /// The worker never started the job. The only error a retry on another
    /// host can take without risking duplicated output.
    NotStarted(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e, what) => write!(f, "rrffmpeg: {what}: {e}"),
            Error::Config(m) => write!(f, "rrffmpeg: config: {m}"),
            Error::State(m) => write!(f, "rrffmpeg: state: {m}"),
            Error::Transport(m) => write!(f, "rrffmpeg: transport: {m}"),
            Error::NotStarted(m) => write!(f, "rrffmpeg: not started: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e, "io".into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One test at a time for the tests that write executables or spawn processes.
///
/// `fork` copies the whole descriptor table, so a child forked by another test
/// thread keeps a script this thread is writing open for writing until it execs.
/// Executing that script then fails with `ETXTBSY`. There is no per-test way to
/// avoid the copy, so those tests take this lock for their whole body.
#[cfg(test)]
pub(crate) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(items: &[&str]) -> Class {
        let args: Vec<OsString> = items.iter().map(OsString::from).collect();
        Class::of(Program::Ffmpeg, &args)
    }

    /// The extraction Jellyfin runs for trickplay: key frames only, MJPEG, a
    /// numbered series. It is the reason the split exists.
    /// Jellyfin probes with `-version` and `-encoders` when it starts: a
    /// listing, a few milliseconds, no input. They must never wait for a
    /// transcode, and they are not transcodes in the stats either.
    #[test]
    fn a_startup_probe_is_light_and_kind_probe() {
        for argv in [
            vec!["-version"],
            vec!["-encoders"],
            vec!["-hwaccels"],
            vec!["-filters"],
            vec!["-h", "encoder=h264_vaapi"],
        ] {
            let job = Job {
                id: "j".into(),
                program: Program::Ffmpeg,
                args: argv.iter().map(std::ffi::OsString::from).collect(),
                priority: 0,
                capabilities: Vec::new(),
            };
            assert_eq!(job.class(), Class::Light, "{argv:?}");
            assert_eq!(job.kind(), Kind::Probe, "{argv:?}");
        }
        // A job that carries an input is never a listing, whatever else it says.
        let with_input = Job {
            id: "j".into(),
            program: Program::Ffmpeg,
            args: ["-h", "1", "-i", "file:/m/x.mkv"]
                .iter()
                .map(std::ffi::OsString::from)
                .collect(),
            priority: 0,
            capabilities: Vec::new(),
        };
        assert_eq!(with_input.class(), Class::Heavy);
    }

    #[test]
    fn a_trickplay_scan_is_light() {
        assert_eq!(
            class(&[
                "-loglevel",
                "error",
                "-skip_frame",
                "nokey",
                "-i",
                "file:/m/ep.mkv",
                "-map",
                "0:0",
                "-an",
                "-sn",
                "-vf",
                "fps=0.1,scale=320:-2",
                "-c:v",
                "mjpeg",
                "-f",
                "image2",
                "/tmp/tp/%08d.jpg",
            ]),
            Class::Light
        );
    }

    #[test]
    fn a_single_frame_thumbnail_is_light() {
        assert_eq!(
            class(&[
                "-i",
                "file:/m/ep.mkv",
                "-vf",
                "scale=320:-2",
                "-frames:v",
                "1",
                "/tmp/t.jpg",
            ]),
            Class::Light
        );
    }

    /// A sprite sheet written through `-f image2` is Light even without the
    /// `-frames:v` and `-skip_frame` pair.
    #[test]
    fn an_image2_output_is_light() {
        assert_eq!(
            class(&["-i", "file:/m/ep.mkv", "-f", "image2", "/tmp/tp/%08d.jpg"]),
            Class::Light
        );
    }

    #[test]
    fn ffprobe_is_light_whatever_the_arguments() {
        let args = vec![OsString::from("-show_streams")];
        assert_eq!(Class::of(Program::Ffprobe, &args), Class::Light);
        assert_eq!(Class::of(Program::Ffprobe, &[]), Class::Light);
    }

    #[test]
    fn an_encode_or_a_remux_is_heavy() {
        assert_eq!(
            class(&[
                "-i",
                "file:/m/ep.mkv",
                "-c:v",
                "h264_vaapi",
                "-f",
                "hls",
                "/tmp/out.m3u8",
            ]),
            Class::Heavy
        );
        assert_eq!(
            class(&[
                "-i",
                "file:/m/ep.mkv",
                "-c",
                "copy",
                "-f",
                "mpegts",
                "/tmp/out.ts",
            ]),
            Class::Heavy
        );
        // A flag that names a value, without being followed by it, is not the
        // pair the rule looks for.
        assert_eq!(class(&["-f"]), Class::Heavy);
        assert_eq!(class(&["-skip_frame", "default"]), Class::Heavy);
    }

    #[test]
    fn a_job_reads_its_class_from_its_own_arguments() {
        let job = Job {
            id: "j".into(),
            program: Program::Ffmpeg,
            args: vec![
                OsString::from("-skip_frame"),
                OsString::from("nokey"),
                OsString::from("-f"),
                OsString::from("image2"),
            ],
            priority: 0,
            capabilities: Vec::new(),
        };
        assert_eq!(job.class(), Class::Light);
        assert_eq!(job.class().name(), "light");
        assert_eq!(Class::Heavy.name(), "heavy");
    }

    fn kind(items: &[&str]) -> Kind {
        let args: Vec<OsString> = items.iter().map(OsString::from).collect();
        Kind::of(Program::Ffmpeg, &args)
    }

    fn job(items: &[&str]) -> Job {
        Job {
            id: "j".into(),
            program: Program::Ffmpeg,
            args: items.iter().map(OsString::from).collect(),
            priority: 0,
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn ffprobe_is_a_probe() {
        let args = vec![OsString::from("-show_streams")];
        assert_eq!(Kind::of(Program::Ffprobe, &args), Kind::Probe);
        assert_eq!(Kind::Probe.name(), "probe");
    }

    #[test]
    fn a_single_frame_is_a_thumbnail() {
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-vf",
                "scale=320:-2",
                "-frames:v",
                "1",
                "/tmp/t.jpg",
            ]),
            Kind::Thumbnail
        );
    }

    #[test]
    fn a_key_frame_scan_is_a_trickplay() {
        assert_eq!(
            kind(&[
                "-loglevel",
                "error",
                "-skip_frame",
                "nokey",
                "-i",
                "file:/m/ep.mkv",
                "-an",
                "-sn",
                "-c:v",
                "mjpeg",
                "-f",
                "image2",
                "/tmp/tp/%08d.jpg",
            ]),
            Kind::Trickplay
        );
    }

    /// Same encoder and format as the trickplay, and still a sprite: only the
    /// key-frame scan makes it a trickplay.
    #[test]
    fn an_image2_writer_without_a_key_frame_scan_is_a_sprite() {
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-vf",
                "fps=1,scale=320:-2",
                "-c:v",
                "mjpeg",
                "-f",
                "image2",
                "/tmp/tp/%08d.jpg",
            ]),
            Kind::Sprite
        );
    }

    #[test]
    fn a_null_output_is_an_analysis() {
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-vf",
                "blackdetect",
                "-f",
                "null",
                "-",
            ]),
            Kind::Analysis
        );
    }

    #[test]
    fn an_audio_extraction_is_audio() {
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-map",
                "0:3",
                "-an",
                "-vn",
                "-c:s",
                "copy",
                "out.srt",
            ]),
            Kind::Audio
        );
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-map",
                "0:a:0",
                "-c:a",
                "aac",
                "out.m4a",
            ]),
            Kind::Audio
        );
    }

    #[test]
    fn a_copied_video_is_a_remux() {
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-c:v",
                "copy",
                "-f",
                "mpegts",
                "out.ts",
            ]),
            Kind::Remux
        );
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-c",
                "copy",
                "-f",
                "matroska",
                "out.mkv",
            ]),
            Kind::Remux
        );
    }

    #[test]
    fn an_encoded_video_is_a_transcode() {
        assert_eq!(
            kind(&[
                "-i",
                "file:/m/ep.mkv",
                "-c:v",
                "hevc_vaapi",
                "-f",
                "hls",
                "out.m3u8",
            ]),
            Kind::Transcode
        );
    }

    /// Class and kind answer different questions. The analysis decodes the
    /// whole file and writes no playback output; the trickplay reads key
    /// frames only, so it costs almost nothing.
    #[test]
    fn the_class_and_the_kind_stay_independent() {
        let analysis = job(&["-i", "file:/m/ep.mkv", "-f", "null", "-"]);
        assert_eq!(analysis.class(), Class::Heavy);
        assert_eq!(analysis.kind(), Kind::Analysis);

        let trickplay = job(&[
            "-skip_frame",
            "nokey",
            "-i",
            "file:/m/ep.mkv",
            "-c:v",
            "mjpeg",
            "-f",
            "image2",
            "/tmp/tp/%08d.jpg",
        ]);
        assert_eq!(trickplay.class(), Class::Light);
        assert_eq!(trickplay.kind(), Kind::Trickplay);
    }

    #[test]
    fn the_input_drops_the_file_scheme() {
        assert_eq!(job(&["-i", "file:/media/x.mkv"]).input(), "/media/x.mkv");
        assert_eq!(job(&["-i", "/media/x.mkv"]).input(), "/media/x.mkv");
        assert_eq!(job(&["-c", "copy"]).input(), "");
    }
}
