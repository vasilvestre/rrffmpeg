//! Two jobs through the dispatcher, then the numbers read back from the
//! journal. The ssh is a script that runs the remote command locally, so the
//! whole path is exercised without a worker.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-stats-{}-{tag}", std::process::id()));
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

fn stats(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rrffmpeg-stats"))
        .args(args)
        .output()
        .expect("rrffmpeg-stats starts")
}

#[test]
fn the_journal_holds_both_jobs_and_stats_reports_them() {
    let root = dir("journal");
    let state = root.join("state");
    fs::create_dir_all(&state).expect("state dir");

    // A stand-in ssh that runs the remote command it was handed. The remote
    // command is the last argument, so `sh -c` is the whole transport.
    let ssh = root.join("ssh");
    executable(
        &ssh,
        "#!/bin/sh\nlast=\nfor a in \"$@\"; do last=$a; done\nexec sh -c \"$last\"\n",
    );

    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            "state_dir = \"{state}\"\nssh_bin = \"{ssh}\"\nssh_persist_secs = 0\nqueue_wait_secs = 2\n\n[[hosts]]\nname = \"worker\"\nconcurrency = 1\nffmpeg_bin = \"/bin/true\"\nffprobe_bin = \"/bin/true\"\n",
            state = state.display(),
            ssh = ssh.display(),
        ),
    )
    .expect("config");

    for _ in 0..2 {
        let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
            .args(["-i", "file:/m/x.mkv", "-c:v", "libx264"])
            .env("RRFFMPEG_CONFIG", &config)
            .output()
            .expect("rrffmpeg starts");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // The drop of the journal flushes before the process exits, so the file
    // is complete now. JSON is the machine answer.
    let out = stats(&["--dir", state.to_str().expect("utf-8"), "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("json output");
    assert_eq!(value["jobs"], 2, "{value}");
    assert!(value["records"].as_u64().expect("records") >= 4, "{value}");

    // The human report is the same data, and must not fail either.
    let out = stats(&["--dir", state.to_str().expect("utf-8")]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("jobs 2"), "{text}");
    assert!(text.contains("host worker"), "{text}");

    // An empty journal prints zeros instead of panicking.
    let empty = root.join("empty");
    fs::create_dir_all(&empty).expect("empty dir");
    let out = stats(&["--dir", empty.to_str().expect("utf-8")]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("records 0"), "{text}");
    assert!(text.contains("jobs 0"), "{text}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_unknown_option_reports_an_error() {
    let out = stats(&["--nope"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown option --nope"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
