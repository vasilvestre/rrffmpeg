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
use rrffmpeg::transport::{Persist, Router, Ssh, Transport};
use rrffmpeg::{Error, Job, Program, Result, capabilities, config::Config, paths};
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

fn main() {
    // The guard must drop before `process::exit`, which skips destructors.
    #[cfg(feature = "hotpath")]
    let guard = hotpath::HotpathGuardBuilder::new("rrffmpeg")
        .functions_limit(12)
        .build();

    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            127
        }
    };

    #[cfg(feature = "hotpath")]
    drop(guard);

    std::process::exit(code);
}

fn run() -> Result<i32> {
    // Monotonic, so the dispatch figure in the journal cannot be distorted by
    // a clock step. Wall clock lives in the journal records themselves.
    let start = std::time::Instant::now();
    let mut argv = std::env::args_os();
    let argv0 = argv.next().unwrap_or_default();
    let args: Vec<OsString> = argv.collect();

    // Answered before the config loads, so a broken config still reports the
    // version running here. Only the real name, never the ffmpeg symlink:
    // `ffmpeg --version` must keep reaching ffmpeg.
    if invoked_as_rrffmpeg(&argv0) && args.len() == 1 && args[0] == "--version" {
        println!("rrffmpeg {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }

    let program = program_for(&argv0, &args);
    let config = Config::load()?;
    // `rrffmpeg capabilities [host]` prints the tokens each worker proves, in
    // the shape `config.example.toml` wants. Run it on the client, against the
    // real workers, after a driver or ffmpeg update.
    if invoked_as_rrffmpeg(&argv0) && args.first().is_some_and(|a| a == "capabilities") {
        return capabilities_command(&config, &args[1..]);
    }
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
        // A client that forbids the fallback refuses even here, or a missing
        // config would quietly encode in software.
        if !config.fallback_local && program == Program::Ffmpeg {
            rrffmpeg::log_at!(
                rrffmpeg::log::Level::Error,
                "no worker configured and fallback_local is false: not encoding on this host"
            );
            return Ok(1);
        }
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
        ssh: ssh_transport(&config)?,
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
        capabilities: capabilities::required(&args),
    };

    let wait = Duration::from_secs(config.queue_wait_secs);
    match dispatch(
        &scheduler,
        &journal,
        transport.as_ref(),
        &job,
        start,
        wait,
        stderr_as_stdout,
    ) {
        Dispatch::Ran(code) => Ok(code),
        Dispatch::NoSlot => {
            // A probe is cheap and Jellyfin needs it for direct-play, so only
            // an ffmpeg job is refused when the fallback is forbidden.
            if !config.fallback_local && program == Program::Ffmpeg {
                rrffmpeg::log_at!(
                    rrffmpeg::log::Level::Error,
                    "no worker slot after {}s and fallback_local is false: not encoding on this host",
                    config.queue_wait_secs
                );
                return Ok(1);
            }
            rrffmpeg::log_at!(
                rrffmpeg::log::Level::Warn,
                "no worker slot after {}s, running locally",
                config.queue_wait_secs
            );
            run_local(&config, program, &args, stderr_as_stdout)
        }
        Dispatch::Failed(e) => Err(e),
    }
}

/// What became of one job, for the caller that owns the local fallback.
enum Dispatch {
    Ran(i32),
    Failed(Error),
    NoSlot,
}

/// Place the job, run it, record it.
///
/// A dispatch that fails before the worker starts ffmpeg cost nothing and left
/// no output behind, so it is retried once on another host: one flaky link
/// must not become an outage. Only the agent can prove that boundary, through
/// its `ready` frame, so only it returns `Error::NotStarted`.
fn dispatch(
    scheduler: &Scheduler,
    journal: &Journal,
    transport: &dyn Transport,
    job: &Job,
    start: std::time::Instant,
    wait: Duration,
    stderr_as_stdout: bool,
) -> Dispatch {
    let can_retry = scheduler.hosts().len() > 1;
    let mut attempt = job.clone();
    let mut failed: Option<Error> = None;
    loop {
        let lease = match scheduler.acquire_blocking(&attempt, wait) {
            Some(lease) => lease,
            // A failed attempt that cannot be retried keeps its own error.
            // Falling back to local here would hide a worker that answered.
            None => return failed.map_or(Dispatch::NoSlot, Dispatch::Failed),
        };

        let mapped = Job {
            args: attempt
                .args
                .iter()
                .map(|a| paths::to_worker(&lease.host, a))
                .collect(),
            ..attempt.clone()
        };

        // Queued, not fsync'd: the transcode starts without waiting for the disk.
        journal.append(Event::Started {
            job: mapped.id.clone(),
            host: lease.host.name.clone(),
            elapsed_us: start.elapsed().as_micros() as u64,
            program: mapped.program.binary().to_string(),
            // This job is not in the journal yet, so the count is one short.
            in_flight: journal.active_on(&lease.host.name) + 1,
        });

        let started = std::time::Instant::now();
        match transport.run(&lease.host, &mapped, stderr_as_stdout) {
            Ok(code) => {
                journal.append(Event::Finished {
                    job: mapped.id.clone(),
                    code,
                    host: lease.host.name.clone(),
                    dur_us: started.elapsed().as_micros() as u64,
                    // Taken here, once, so a retry below starts with an empty
                    // slot instead of inheriting the failed attempt's lines.
                    stats: Box::new(rrffmpeg::progress::take()),
                });
                // Free the slot before returning so the next job can start at once.
                drop(lease);
                return Dispatch::Ran(code);
            }
            Err(e) => {
                journal.append(Event::HostDown {
                    host: lease.host.name.clone(),
                });
                scheduler.mark_down(&lease.host.name);
                drop(lease);
                if can_retry && failed.is_none() && matches!(e, Error::NotStarted(_)) {
                    failed = Some(e);
                    // A fresh id keeps one journal row per attempt, so the
                    // counters and `rrffmpeg-stats` see two jobs, not one.
                    attempt = Job {
                        id: job_id(),
                        ..job.clone()
                    };
                    continue;
                }
                return Dispatch::Failed(e);
            }
        }
    }
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
    let err = Command::new(&bin).args(args).envs(&config.local_env).exec();
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

/// SSH transport, with one persistent connection per host when configured.
/// Prints one TOML `capabilities` row per host, or one host when named.
/// Returns 1 when a probe fails, so a script can tell "no GPU" from "quiet".
fn capabilities_command(config: &Config, args: &[OsString]) -> Result<i32> {
    let wanted = args.first().and_then(|a| a.to_str());
    let ssh = ssh_transport(config)?;
    let mut failed = false;
    let mut probed = 0;
    for host in &config.hosts {
        if wanted.is_some_and(|w| w != host.name) {
            continue;
        }
        probed += 1;
        match ssh.probe_capabilities(host) {
            Ok(tokens) => {
                let list: Vec<String> = tokens.iter().map(|t| format!("\"{t}\"")).collect();
                println!("# {}\ncapabilities = [{}]", host.name, list.join(", "));
            }
            Err(e) => {
                eprintln!("{e}");
                failed = true;
            }
        }
    }
    if probed == 0 {
        return Err(Error::Config(format!(
            "no host matches {:?}",
            wanted.unwrap_or("")
        )));
    }
    Ok(if failed { 1 } else { 0 })
}

fn ssh_transport(config: &Config) -> Result<Ssh> {
    let mut ssh = Ssh::new(config.ssh_bin.clone());

    ssh.ssh_args = config.ssh_args.clone();
    if config.ssh_persist_secs == 0 {
        return Ok(ssh);
    }
    ssh.persist = Some(Persist {
        dir: ssh_socket_dir(config)?,
        secs: config.ssh_persist_secs,
    });
    Ok(ssh)
}

/// A control socket dies with the session, so it belongs on tmpfs. The session
/// runtime directory wins, then `/dev/shm`, then `state_dir` for a box with
/// neither. Each candidate is private: ssh trusts whoever can open the socket.
fn ssh_socket_dir(config: &Config) -> Result<PathBuf> {
    if let Some(dir) = &config.ssh_socket_dir {
        return private_dir(dir);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return private_dir(&PathBuf::from(runtime).join("rrffmpeg"));
    }
    let uid = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0);
    private_dir(&PathBuf::from(format!("/dev/shm/rrffmpeg-{uid}")))
        .or_else(|_| private_dir(&config.state_dir.join("ssh")))
}

fn private_dir(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Io(e, format!("creating {}", dir.display())))?;
    // Refuse a symlink or a directory someone else owns: another local user
    // could otherwise plant a name we then trust.
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| Error::Io(e, format!("stat {}", dir.display())))?;
    let uid = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0);
    if !meta.is_dir() || meta.uid() != uid {
        return Err(Error::Config(format!(
            "{} is not a directory this user owns",
            dir.display()
        )));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::Io(e, format!("setting mode on {}", dir.display())))?;
    Ok(dir.to_path_buf())
}

fn invoked_as_rrffmpeg(argv0: &OsString) -> bool {
    std::path::Path::new(argv0)
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("rrffmpeg"))
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

    /// Fails the first dispatch only, then succeeds: the shape of a lost
    /// handshake on one worker while another is healthy.
    struct Flaky {
        calls: std::sync::Mutex<Vec<String>>,
        failed_once: std::sync::Mutex<bool>,
    }

    impl Transport for Flaky {
        fn run(&self, host: &rrffmpeg::Host, _job: &Job, _stderr_as_stdout: bool) -> Result<i32> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(host.name.clone());
            let mut failed = self.failed_once.lock().unwrap_or_else(|e| e.into_inner());
            if *failed {
                return Ok(0);
            }
            *failed = true;
            Err(Error::NotStarted("no ready frame".into()))
        }

        fn probe(&self, _host: &rrffmpeg::Host) -> bool {
            true
        }

        fn job_alive(&self, _host: &rrffmpeg::Host, _job: &str) -> bool {
            false
        }
    }

    fn host(name: &str) -> rrffmpeg::Host {
        rrffmpeg::Host {
            name: name.into(),
            concurrency: 1,
            capabilities: Vec::new(),
            ffmpeg_bin: None,
            ffprobe_bin: None,
            agent_addr: None,
            ssh_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            paths: Vec::new(),
        }
    }

    #[test]
    fn a_job_that_never_started_moves_to_another_host() {
        let dir = std::env::temp_dir().join(format!("rrffmpeg-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("state dir");
        let scheduler = Scheduler::new(
            vec![host("a"), host("b")],
            Arc::new(LockDir::new(&dir)),
            Duration::from_secs(60),
        );
        let journal = Journal::open(&dir).expect("journal");
        let flaky = Flaky {
            calls: std::sync::Mutex::new(Vec::new()),
            failed_once: std::sync::Mutex::new(false),
        };
        let job = Job {
            id: "retry-1".into(),
            program: Program::Ffmpeg,
            args: args(&["-i", "file:/m/x.mkv"]),
            priority: 0,
            capabilities: Vec::new(),
        };

        let got = dispatch(
            &scheduler,
            &journal,
            &flaky,
            &job,
            std::time::Instant::now(),
            Duration::from_secs(5),
            false,
        );

        assert!(matches!(got, Dispatch::Ran(0)));
        let calls = flaky.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 2, "one retry, not a loop");
        assert_ne!(calls[0], calls[1], "the retry must leave the failed host");
        assert!(
            scheduler.is_down(&calls[0]),
            "the failed host is marked down"
        );
        drop(calls);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
