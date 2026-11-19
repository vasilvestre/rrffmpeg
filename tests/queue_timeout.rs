//! What a job does when the only worker slot stays taken.
//!
//! The test process holds the slot through the same flock file the dispatcher
//! reads, so the child sees a busy worker without an ssh or an agent.

use rrffmpeg::config::Host;
use rrffmpeg::locks::LockDir;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-queue-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perm = fs::metadata(path).expect("stat script").permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).expect("chmod script");
}

fn worker() -> Host {
    Host {
        name: "worker".into(),
        concurrency: 1,
        capabilities: Vec::new(),
        local: false,
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

fn run(config: &Path, bin: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.args(args)
        .env("RRFFMPEG_CONFIG", config)
        .env("RRFFMPEG_BIN", bin)
        .env_remove("RRFFMPEG_PROGRAM")
        .stdin(Stdio::null());
    for (key, value) in extra {
        cmd.env(key, value);
    }
    cmd.output().expect("rrffmpeg starts")
}

#[test]
fn a_busy_worker_falls_back_or_refuses() {
    let root = dir("busy");
    let state = root.join("state");
    fs::create_dir_all(&state).expect("state dir");

    // Hold the only slot for the length of the test. A second process, the
    // client, sees it as busy: flock is per open file description.
    let locks = LockDir::new(&state);
    let _hold = locks
        .try_acquire(&worker(), "holder")
        .expect("the only slot is free");

    // The local stand-in writes a marker, so a fallback is visible.
    let marker = root.join("ran");
    let bin = root.join("local");
    executable(
        &bin,
        &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
    );

    let transcode = ["-i", "file:/m/x.mkv", "-c:v", "libx264"];

    let allowed = root.join("allowed.toml");
    fs::write(
        &allowed,
        format!(
            "state_dir = \"{}\"\nqueue_wait_secs = 0\nfallback_local = true\nssh_persist_secs = 0\n\n[[hosts]]\nname = \"worker\"\n",
            state.display()
        ),
    )
    .expect("allowed config");

    let denied = root.join("denied.toml");
    fs::write(
        &denied,
        format!(
            "state_dir = \"{}\"\nqueue_wait_secs = 0\nfallback_local = false\nssh_persist_secs = 0\n\n[[hosts]]\nname = \"worker\"\n",
            state.display()
        ),
    )
    .expect("denied config");

    // fallback_local = true: the local binary runs after the wait of zero.
    let out = run(&allowed, &bin, &transcode, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(marker.exists(), "the local binary did not run");
    fs::remove_file(&marker).expect("clear marker");

    // fallback_local = false: no transcode on this host, exit 1.
    let out = run(&denied, &bin, &transcode, &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!marker.exists(), "a forbidden fallback still encoded");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fallback_local"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An ffprobe is cheap and Jellyfin needs it, so it still runs locally.
    let out = run(
        &denied,
        &bin,
        &["-v", "error", "-show_format", "file:/m/x.mkv"],
        &[("RRFFMPEG_PROGRAM", "ffprobe")],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(marker.exists(), "a local probe did not run");

    drop(_hold);
    let _ = fs::remove_dir_all(&root);
}
