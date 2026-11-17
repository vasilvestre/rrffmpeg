//! Stand in for `ffmpeg`/`ffprobe`, place the job on a worker, and keep the
//! bookkeeping off the critical path.
//!
//! With no worker configured the process replaces itself with the local
//! binary, exactly like the first version: arguments, stdio, exit code and
//! signals pass through untouched.

use rrffmpeg::janitor::Janitor;
use rrffmpeg::scheduler::Scheduler;
use rrffmpeg::state::{Event, Journal};
use rrffmpeg::transport::{Ssh, Transport};
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
    let mut argv = std::env::args_os();
    let argv0 = argv.next().unwrap_or_default();
    let args: Vec<OsString> = argv.collect();

    let program = program_for(&argv0);
    let config = Config::load()?;
    let stderr_as_stdout =
        config.stderr_as_stdout || std::env::var_os("RRFFMPEG_STDERR_AS_STDOUT").is_some();

    if config.hosts.is_empty() {
        // Local-only mode: no state, no thread, nothing on the critical path.
        return run_local(&config, program, &args, stderr_as_stdout);
    }

    let journal = Arc::new(Journal::open(&config.state_dir)?);
    let scheduler = Arc::new(Scheduler::new(config.hosts.clone()));
    let transport: Arc<dyn Transport> = Arc::new(Ssh::new(config.ssh_bin.clone()));
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

    let lease = match scheduler.acquire_blocking(&job, Duration::from_secs(30)) {
        Some(lease) => lease,
        None => {
            eprintln!("rrffmpeg: no worker slot after 30s, running locally");
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
    });

    let code = match transport.run(&lease.host, &mapped, stderr_as_stdout) {
        Ok(code) => code,
        Err(e) => {
            journal.append(Event::HostDown {
                host: lease.host.name.clone(),
            });
            scheduler.mark_down(&lease.host.name);
            scheduler.release(lease);
            return Err(e);
        }
    };

    journal.append(Event::Finished {
        job: job.id.clone(),
        code,
    });
    scheduler.release(lease);
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

fn program_for(argv0: &OsString) -> Program {
    if let Some(p) = std::env::var_os("RRFFMPEG_PROGRAM") {
        if p.eq_ignore_ascii_case("ffprobe") {
            return Program::Ffprobe;
        }
        return Program::Ffmpeg;
    }
    // Jellyfin rewrites EncoderPath, so the binary name is the only hint.
    let name = std::path::Path::new(argv0)
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("probe") {
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
