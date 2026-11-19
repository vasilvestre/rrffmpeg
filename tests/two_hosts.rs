//! Two workers, one job: a lost handshake moves the job, a real failure does not.
//!
//! Only the agent transport can prove that ffmpeg never started (its `ready`
//! frame), so these tests run two real daemons on loopback. The first daemon
//! is pointed at a binary that does not exist, so its `exec` fails before any
//! child runs.

use rrffmpeg::agent::{generate_cert, serve_on, server_config};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const TOKEN: &str = "two-hosts-token";
const JOB_ARGS: [&str; 2] = ["-i", "file:/home/alice/media/movie.mkv"];

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "rrffmpeg-two-hosts-{}-{n}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
    path.to_path_buf()
}

/// Start one daemon on an ephemeral loopback port. The certificate must name
/// the address the client verifies.
fn start_agent(cert: &Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let tls = server_config(cert, &cert.with_file_name("key.pem")).expect("tls");
    std::thread::spawn(move || {
        let _ = serve_on(listener, TOKEN.to_string(), tls);
    });
    addr
}

fn run(config: &Path, dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.args(args)
        .env("RRFFMPEG_CONFIG", config)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_STATE_HOME", dir)
        .env_remove("RRFFMPEG_BIN")
        .env_remove("RRFFMPEG_STDERR_AS_STDOUT")
        .env_remove("RRFFMPEG_PROGRAM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.output().expect("rrffmpeg should run")
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

/// The first host fails before its child starts, so the retry runs on the
/// second. The second exit code is the one the client returns, and the journal
/// holds one Started row per attempt, with distinct ids. `prefer` puts the
/// broken host first, because the placement order no longer follows the file.
#[test]
fn a_lost_handshake_moves_the_job_to_the_second_host() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = scratch("retry");
    let state = dir.join("state");
    let cert = dir.join("cert.pem");
    generate_cert(&dir, &["127.0.0.1".to_string()]).expect("cert");

    let first = start_agent(&cert);
    let second = start_agent(&cert);
    // Half a second of work keeps the flusher ahead of the process exit.
    let ok_worker = script(&dir.join("worker-ok"), "#!/bin/sh\nsleep 0.5\nexit 42\n");

    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "state_dir = \"{state}\"\n\
             agent_token = \"{TOKEN}\"\n\
             agent_cert = \"{cert}\"\n\
             queue_wait_secs = 5\n\
             down_ttl_secs = 5\n\
             \n\
             [[hosts]]\n\
             name = \"worker\"\n\
             concurrency = 1\n\
             prefer = 1\n\
             ffmpeg_bin = \"/nonexistent/ffmpeg-never-starts\"\n\
             agent_addr = \"{first}\"\n\
             \n\
             [[hosts]]\n\
             name = \"spare\"\n\
             concurrency = 1\n\
             ffmpeg_bin = \"{ok}\"\n\
             agent_addr = \"{second}\"\n",
            state = state.display(),
            cert = cert.display(),
            ok = ok_worker.display(),
        ),
    )
    .expect("write config");

    let out = run(&config, &dir, &JOB_ARGS);
    assert_eq!(
        out.status.code(),
        Some(42),
        "the second host's exit code must reach the client: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let records = rrffmpeg::state::records(&state).expect("read journal");
    let started: Vec<(&str, &str)> = records
        .iter()
        .filter_map(|record| match &record.ev {
            rrffmpeg::state::Event::Started { job, host, .. } => {
                Some((job.as_str(), host.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 2, "one Started row per attempt: {started:?}");
    assert_ne!(started[0].0, started[1].0, "each attempt gets a fresh id");
    let hosts: Vec<&str> = started.iter().map(|(_, host)| *host).collect();
    assert!(hosts.contains(&"worker"), "the first attempt ran on worker");
    assert!(hosts.contains(&"spare"), "the retry ran on spare");

    let _ = std::fs::remove_dir_all(&dir);
}

/// When both hosts refuse to start the job, the client fails. It must not
/// quietly encode on a host with no GPU.
#[test]
fn two_failed_hosts_fail_the_job_and_skip_the_local_fallback() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = scratch("both-fail");
    let state = dir.join("state");
    let cert = dir.join("cert.pem");
    generate_cert(&dir, &["127.0.0.1".to_string()]).expect("cert");

    let first = start_agent(&cert);
    let second = start_agent(&cert);
    let marker = dir.join("local-ran");
    let local = script(
        &dir.join("local-ffmpeg"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "state_dir = \"{state}\"\n\
             agent_token = \"{TOKEN}\"\n\
             agent_cert = \"{cert}\"\n\
             queue_wait_secs = 5\n\
             fallback_local = true\n\
             local_ffmpeg = \"{local}\"\n\
             local_ffprobe = \"{local}\"\n\
             \n\
             [[hosts]]\n\
             name = \"worker\"\n\
             concurrency = 1\n\
             ffmpeg_bin = \"/nonexistent/ffmpeg-a\"\n\
             agent_addr = \"{first}\"\n\
             \n\
             [[hosts]]\n\
             name = \"spare\"\n\
             concurrency = 1\n\
             ffmpeg_bin = \"/nonexistent/ffmpeg-b\"\n\
             agent_addr = \"{second}\"\n",
            state = state.display(),
            cert = cert.display(),
            local = local.display(),
        ),
    )
    .expect("write config");

    let out = run(&config, &dir, &JOB_ARGS);
    assert_eq!(
        out.status.code(),
        Some(127),
        "both hosts failing must fail the job: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not started"),
        "the error must name the failed start: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_within(Duration::from_millis(200), || !marker.exists()),
        "the local fallback ran after a worker answered"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Scripts are written while other tests may fork. In this file only these two
/// tests write executable files, so a file-local lock keeps `ETXTBSY` away.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
