//! Two pools, end to end: a Light job never queues behind a transcode, and a
//! killed dispatcher leaves neither a child nor a held slot behind.
//!
//! Each test drives the real binary from a private state dir, so the slot
//! files and the queue are the ones production uses.

use rrffmpeg::Host;
use rrffmpeg::locks::LockDir;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A transcode: Heavy, and it wants the transcode slot.
const HEAVY: &[&str] = &["-i", "file:/m/ep.mkv", "-c:v", "libx264", "-f", "hls"];

/// The trickplay extraction Jellyfin runs: Light, and it must not wait.
const LIGHT: &[&str] = &[
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
];

fn dir(tag: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("rrffmpeg-light-pool-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");
    root
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    let mut perm = std::fs::metadata(path).expect("stat script").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).expect("chmod script");
}

/// A local host, one heavy slot, one light slot: the production shape.
fn host() -> Host {
    Host {
        name: "here".into(),
        concurrency: 1,
        light_concurrency: 1,
        capabilities: Vec::new(),
        local: true,
        ffmpeg_bin: None,
        ffprobe_bin: None,
        agent_addr: None,
        ssh_args: Vec::new(),
        env: BTreeMap::new(),
        paths: Vec::new(),
        reads: Vec::new(),
        prefer: 0,
    }
}

fn config(root: &Path, bin: &Path, wait_secs: u64) -> PathBuf {
    let state = root.join("state");
    std::fs::create_dir_all(&state).expect("state dir");
    let path = root.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "state_dir = \"{}\"\n\
             queue_wait_secs = {wait_secs}\n\
             fallback_local = true\n\
             ssh_persist_secs = 0\n\
             local_ffmpeg = \"{}\"\n\
             local_ffprobe = \"{}\"\n\
             \n\
             [[hosts]]\n\
             name = \"here\"\n\
             local = true\n\
             concurrency = 1\n\
             light_concurrency = 1\n",
            state.display(),
            bin.display(),
            bin.display(),
        ),
    )
    .expect("write config");
    path
}

fn run(config: &Path, args: &[&str]) -> (i32, Duration, String) {
    let began = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(args)
        .env("RRFFMPEG_CONFIG", config)
        .env_remove("RRFFMPEG_PROGRAM")
        .env_remove("RRFFMPEG_BIN")
        .stdin(Stdio::null())
        .output()
        .expect("rrffmpeg starts");
    let code = out.status.code().unwrap_or(-1);
    (
        code,
        began.elapsed(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Hold a slot for the length of the test, the way a running job does.
fn hold(state: &Path, job: &str, class: rrffmpeg::Class) -> rrffmpeg::locks::SlotGuard {
    LockDir::new(state)
        .try_acquire(&host(), job, class)
        .expect("the slot is free")
}

#[test]
fn a_light_job_starts_while_the_only_heavy_slot_is_held() {
    let root = dir("light");
    let marker = root.join("ran");
    let bin = root.join("ffmpeg");
    executable(
        &bin,
        &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
    );
    let config = config(&root, &bin, 3);
    let _heavy = hold(&root.join("state"), "transcode", rrffmpeg::Class::Heavy);

    let (code, elapsed, stderr) = run(&config, LIGHT);
    let ran = marker.exists();
    drop(_heavy);
    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(code, 0, "{stderr}");
    assert!(ran, "the light job did not run");
    assert!(
        elapsed < Duration::from_secs(1),
        "the light job waited {elapsed:?}"
    );
}

#[test]
fn a_light_job_runs_here_when_its_pool_is_full() {
    let root = dir("full");
    let marker = root.join("ran");
    let bin = root.join("ffmpeg");
    executable(
        &bin,
        &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
    );
    let config = config(&root, &bin, 3);
    let state = root.join("state");
    let _heavy = hold(&state, "transcode", rrffmpeg::Class::Heavy);
    let _light = hold(&state, "thumbnail", rrffmpeg::Class::Light);

    let (code, elapsed, stderr) = run(&config, LIGHT);
    let ran = marker.exists();
    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(code, 0, "{stderr}");
    assert!(ran, "the local binary did not run");
    assert!(
        elapsed < Duration::from_secs(1),
        "the light job waited for queue_wait_secs: {elapsed:?}"
    );
}

#[test]
fn a_killed_dispatcher_leaves_no_child_and_no_held_slot() {
    let root = dir("killed");
    let state = root.join("state");
    let first = root.join("first.pid");
    let second = root.join("second");
    let bin = root.join("ffmpeg");
    // SIGTERM is ignored on purpose: ffmpeg can spend a long time writing its
    // trailer, which is the "child outlives the wrapper" this must not leave.
    executable(
        &bin,
        &format!(
            "#!/bin/sh\ntrap '' TERM\necho $$ > '{}'\nexec sleep 300\n",
            first.display()
        ),
    );
    let config = config(&root, &bin, 5);
    std::fs::create_dir_all(&state).expect("state dir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(HEAVY)
        .env("RRFFMPEG_CONFIG", &config)
        .env_remove("RRFFMPEG_BIN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("rrffmpeg starts");

    // Wait for the local ffmpeg to exist and to hold the slot.
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&first)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the child never started");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(alive(pid), "the child is not running");

    // Jellyfin's cancel: SIGKILL to the process it started.
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    assert!(
        gone(pid, Duration::from_secs(2)),
        "the child outlived the dispatcher"
    );

    // The next job must start at once, not after the stale slot is noticed.
    executable(
        &bin,
        &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", second.display()),
    );
    let began = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(HEAVY)
        .env("RRFFMPEG_CONFIG", &config)
        .env_remove("RRFFMPEG_BIN")
        .stdin(Stdio::null())
        .output()
        .expect("rrffmpeg starts");
    let elapsed = began.elapsed();
    let ran = second.exists();
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(ran, "the next job did not run");
    assert!(
        elapsed < Duration::from_secs(1),
        "the next job waited {elapsed:?}"
    );
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A killed process is gone as soon as the kernel reaps it, so a short poll is
/// enough and a slow one would hide a child that never dies.
fn gone(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}
