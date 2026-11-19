//! Signals and a dead client.
//!
//! Local mode replaces this process with ffmpeg, so a signal reaches the child
//! untouched. Worker mode has no signal forwarding: the client dies and the
//! daemon only notices through the stdin connection, which the tests open.

use rrffmpeg::agent::{generate_cert, serve_on, server_config};
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const TOKEN: &str = "signals-token";

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("rrffmpeg-signals-{}-{n}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn wait_within(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

fn command_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn kill_pid(pid: u32, signal: i32) {
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

fn rrffmpeg(config: &Path, dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.env("RRFFMPEG_CONFIG", config)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_STATE_HOME", dir)
        .env_remove("RRFFMPEG_BIN")
        .env_remove("RRFFMPEG_STDERR_AS_STDOUT")
        .env_remove("RRFFMPEG_PROGRAM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// With no hosts the process execs the local binary, so the pid the caller
/// spawned is the child, and each signal ends it with that signal.
#[test]
fn signals_reach_the_local_child() {
    let dir = scratch("local");
    let config = dir.join("local.toml");
    std::fs::write(&config, "").expect("write config");

    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let mut child = rrffmpeg(&config, &dir)
            .env("RRFFMPEG_BIN", "/bin/sleep")
            .args(["60"])
            .spawn()
            .expect("rrffmpeg should start");
        let pid = child.id();
        assert!(
            wait_within(Duration::from_secs(5), || command_of(pid) == "sleep"),
            "rrffmpeg never exec'd the local child (comm {:?})",
            command_of(pid)
        );

        kill_pid(pid, signal);
        let status = child.wait().expect("rrffmpeg should exit");
        assert_eq!(
            status.signal(),
            Some(signal),
            "signal {signal} did not reach the local child"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker script that records its pid, then replaces itself with a sleep.
/// The daemon kills that pid when the client's stdin connection drops.
const WORKER: &str = r#"#!/bin/sh
printf '%s\n' "$$" > '@LOG@'
exec sleep 60
"#;

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

/// The worker process does not outlive a client that opened its stdin
/// connection: the daemon kills it when that connection closes.
///
/// Gap: there is no remote signal forwarding. A job that never writes a byte
/// of stdin leaves no connection to notice, so its worker keeps running. This
/// test covers the path the code does guarantee.
#[test]
fn a_killed_client_does_not_leave_its_worker_running() {
    let dir = scratch("worker");
    let state = dir.join("state");
    let cert = dir.join("cert.pem");
    generate_cert(&dir, &["127.0.0.1".to_string()]).expect("cert");

    let log = dir.join("worker.log");
    let worker = dir.join("worker-ffmpeg");
    std::fs::write(&worker, WORKER.replace("@LOG@", &log.display().to_string()))
        .expect("write worker");
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755))
        .expect("chmod worker");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let tls = server_config(&cert, &dir.join("key.pem")).expect("tls");
    std::thread::spawn(move || {
        let _ = serve_on(listener, TOKEN.to_string(), tls);
    });

    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "state_dir = \"{state}\"\n\
             agent_token = \"{TOKEN}\"\n\
             agent_cert = \"{cert}\"\n\
             queue_wait_secs = 10\n\
             \n\
             [[hosts]]\n\
             name = \"worker\"\n\
             concurrency = 1\n\
             ffmpeg_bin = \"{worker}\"\n\
             agent_addr = \"{addr}\"\n",
            state = state.display(),
            cert = cert.display(),
            worker = worker.display(),
        ),
    )
    .expect("write config");

    let mut client: Child = rrffmpeg(&config, &dir)
        .stdin(Stdio::piped())
        .arg("-i")
        .arg("file:/home/alice/media/movie.mkv")
        .spawn()
        .expect("client should start");

    // One byte opens the stdin connection; keeping the pipe open keeps it up.
    let mut stdin = client.stdin.take().expect("client stdin");
    stdin.write_all(b"x").expect("write one stdin byte");
    stdin.flush().expect("flush stdin");

    let started = wait_within(Duration::from_secs(10), || read_pid(&log).is_some());
    let worker_pid = read_pid(&log);
    client.kill().expect("kill the client");
    let _ = client.wait();

    let Some(worker_pid) = worker_pid else {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("the daemon never started the worker (log {:?})", started);
    };
    let gone = wait_within(Duration::from_secs(8), || !pid_alive(worker_pid));
    if pid_alive(worker_pid) {
        kill_pid(worker_pid, libc::SIGKILL);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        gone,
        "worker {worker_pid} was still alive after its client died"
    );
}
