//! Stand in for `ffmpeg`/`ffprobe`, place the job on a worker, and keep the
//! bookkeeping off the critical path.
//!
//! With no worker configured the process replaces itself with the local
//! binary, exactly like the first version: arguments, stdio, exit code and
//! signals pass through untouched.

use rrffmpeg::janitor::Janitor;
use rrffmpeg::locks::LockDir;
use rrffmpeg::scheduler::Scheduler;
use rrffmpeg::state::{Event, Journal};
use rrffmpeg::transport::{Router, Ssh, Transport};
use rrffmpeg::{Error, Job, Program, Result, config::Config, paths};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            127
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    // Monotonic, so the dispatch figure in the journal cannot be distorted by
    // a clock step. Wall clock lives in the journal records themselves.
    let start = std::time::Instant::now();
    let mut argv = std::env::args_os();
    let argv0 = argv.next().unwrap_or_default();
    let args: Vec<OsString> = argv.collect();

    let program = program_for(&argv0, &args);
    let config = Config::load()?;
    let agent = agent_client(&config)?;
    if agent.is_none() && config.hosts.iter().any(|h| h.agent_addr.is_some()) {
        return Err(Error::Config(
            "a host sets agent_addr but the agent is not configured: set agent_token and agent_cert, or RRFFMPEG_AGENT_TOKEN".into(),
        ));
    }
    let stderr_as_stdout =
        config.stderr_as_stdout || std::env::var_os("RRFFMPEG_STDERR_AS_STDOUT").is_some();

    if config.hosts.is_empty() {
        // Local-only mode: no state, no thread, nothing on the critical path.
        return run_local(&config, program, &args, stderr_as_stdout);
    }

    let journal = Arc::new(Journal::open(&config.state_dir)?);
    let scheduler = Arc::new(
        Scheduler::new(
            config.hosts.clone(),
            Arc::new(LockDir::new(&config.state_dir)),
            Duration::from_secs(config.down_ttl_secs),
        )
        .with_client(config.client_name.clone(), config.client_weight),
    );
    let transport: Arc<dyn Transport> = Arc::new(Router {
        ssh: Ssh::new(config.ssh_bin.clone()),
        agent,
    });
    let _janitor = Janitor::spawn(
        scheduler.clone(),
        journal.clone(),
        transport.clone(),
        Duration::from_secs(10),
    );

    let job = Job {
        id: job_id(),
        program,
        args: args.clone(),
        // Probes and seeks jump the queue; a transcode only starts when a slot frees.
        priority: if program == Program::Ffprobe { -5 } else { 0 },
        capability: required_capability(&args),
    };

    let wait = Duration::from_secs(config.queue_wait_secs);
    let lease = match scheduler.acquire_blocking(&job, wait) {
        Some(lease) => lease,
        None => {
            eprintln!(
                "rrffmpeg: no worker slot after {}s, running locally",
                config.queue_wait_secs
            );
            return run_local(&config, program, &args, stderr_as_stdout);
        }
    };

    let mapped = Job {
        args: args
            .iter()
            .map(|a| paths::to_worker(&lease.host, a))
            .collect(),
        ..job.clone()
    };

    // Queued, not fsync'd: the transcode starts without waiting for the disk.
    journal.append(Event::Started {
        job: job.id.clone(),
        host: lease.host.name.clone(),
        elapsed_us: start.elapsed().as_micros() as u64,
    });

    let code = match transport.run(&lease.host, &mapped, stderr_as_stdout) {
        Ok(code) => code,
        Err(e) => {
            journal.append(Event::HostDown {
                host: lease.host.name.clone(),
            });
            scheduler.mark_down(&lease.host.name);
            drop(lease);
            return Err(e);
        }
    };

    journal.append(Event::Finished {
        job: job.id.clone(),
        code,
    });
    // Free the slot before returning so the next job can start at once.
    drop(lease);
    Ok(code)
}

fn run_local(
    config: &Config,
    program: Program,
    args: &[OsString],
    stderr_as_stdout: bool,
) -> Result<i32> {
    if stderr_as_stdout {
        unsafe { libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) };
    }
    // RRFFMPEG_BIN overrides the local tool, for tests and for odd installs.
    let bin = std::env::var_os("RRFFMPEG_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config.local_binary(program).clone());
    let err = Command::new(&bin).args(args).exec();
    Err(Error::Io(err, format!("cannot run {}", bin.display())))
}

/// The environment wins over the file so a secret can stay out of a config
/// file that other users can read. Both halves are needed: the token says who
/// may connect, the certificate says which daemon to trust.
fn agent_client(config: &Config) -> Result<Option<rrffmpeg::agent::Agent>> {
    let token = std::env::var("RRFFMPEG_AGENT_TOKEN")
        .ok()
        .or_else(|| config.agent_token.clone())
        .filter(|t| !t.is_empty());
    match (token, config.agent_cert.clone()) {
        (Some(token), Some(cert)) => Ok(Some(rrffmpeg::agent::Agent::load(token, &cert)?)),
        (None, None) => Ok(None),
        _ => Err(Error::Config(
            "the agent transport needs both agent_token and agent_cert".into(),
        )),
    }
}

/// The encoder in the arguments tells us which worker can run the job.
/// Jellyfin passes `-c:v h264_vaapi`, `hevc_nvenc`, `h264_qsv` and friends.
fn required_capability(args: &[OsString]) -> Option<String> {
    for arg in args {
        let text = arg.to_string_lossy();
        for cap in ["vaapi", "nvenc", "qsv", "amf", "videotoolbox"] {
            if text.ends_with(cap) || text.ends_with(&format!("_{cap}")) {
                return Some(cap.to_string());
            }
        }
    }
    None
}

fn program_for(argv0: &OsString, args: &[OsString]) -> Program {
    if let Some(p) = std::env::var_os("RRFFMPEG_PROGRAM") {
        if p.eq_ignore_ascii_case("ffprobe") {
            return Program::Ffprobe;
        }
        return Program::Ffmpeg;
    }
    // Jellyfin rewrites EncoderPath, so the binary name is the first hint.
    let name = std::path::Path::new(argv0)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("probe") {
        return Program::Ffprobe;
    }
    // One name still gets both tools, so fall back to the argument shape.
    const PROBE_FLAGS: [&str; 8] = [
        "-print_format",
        "-show_streams",
        "-show_format",
        "-show_chapters",
        "-show_frames",
        "-show_entries",
        "-select_streams",
        "-show_packets",
    ];
    let looks_like_probe = args.iter().any(|a| {
        let text = a.to_string_lossy();
        PROBE_FLAGS.contains(&text.as_ref())
    });
    if looks_like_probe {
        Program::Ffprobe
    } else {
        Program::Ffmpeg
    }
}

/// Unique across restarts, so a stale row cannot be confused with this job.
fn job_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_binary_name_picks_the_tool() {
        assert_eq!(
            program_for(&OsString::from("/usr/local/bin/ffprobe"), &[]),
            Program::Ffprobe
        );
        assert_eq!(
            program_for(&OsString::from("/usr/local/bin/ffmpeg"), &[]),
            Program::Ffmpeg
        );
    }

    #[test]
    fn probe_arguments_pick_ffprobe_on_a_single_name_install() {
        let a = args(&[
            "-v",
            "warning",
            "-print_format",
            "json",
            "-show_streams",
            "file:/m/x.mkv",
        ]);
        assert_eq!(
            program_for(&OsString::from("/usr/local/bin/rrffmpeg"), &a),
            Program::Ffprobe
        );
    }

    #[test]
    fn a_transcode_stays_ffmpeg() {
        let a = args(&[
            "-i",
            "file:/m/x.mkv",
            "-c:v",
            "h264_vaapi",
            "-f",
            "hls",
            "/tmp/out.m3u8",
        ]);
        assert_eq!(
            program_for(&OsString::from("/usr/local/bin/rrffmpeg"), &a),
            Program::Ffmpeg
        );
    }

    #[test]
    fn capability_comes_from_the_encoder_name() {
        assert_eq!(
            required_capability(&args(&["-c:v", "hevc_vaapi", "-i", "/m/x.mkv"])),
            Some("vaapi".into())
        );
        assert_eq!(
            required_capability(&args(&["-hwaccel", "nvenc", "-i", "/m/x.mkv"])),
            Some("nvenc".into())
        );
        assert_eq!(
            required_capability(&args(&["-c:v", "libx264", "-i", "/m/x.mkv"])),
            None
        );
        assert_eq!(required_capability(&args(&["-c", "copy"])), None);
    }
}
