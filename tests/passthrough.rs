//! The dispatch contract: args, stdio and exit code must survive unchanged.
//! Each test runs the real binary with RRFFMPEG_BIN pointing at a stand-in.

use std::path::Path;
use std::process::Command;

fn run(bin: &str, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.args(args).env("RRFFMPEG_BIN", bin);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("rrffmpeg should start")
}

#[test]
fn args_reach_the_child_unchanged() {
    let out = run(
        "sh",
        &[
            "-c",
            "printf '%s\\n' \"$@\"",
            "sh",
            "a b",
            "-c:v",
            "libx264",
        ],
        &[],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b\n-c:v\nlibx264\n");
}

#[test]
fn exit_code_is_passed_through() {
    assert_eq!(run("sh", &["-c", "exit 7"], &[]).status.code(), Some(7));
}

#[test]
fn stderr_stays_on_stderr_by_default() {
    let out = run("sh", &["-c", "echo out; echo err >&2"], &[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err\n");
}

#[test]
fn stderr_as_stdout_moves_both_to_stdout() {
    let out = run(
        "sh",
        &["-c", "echo out; echo err >&2"],
        &[("RRFFMPEG_STDERR_AS_STDOUT", "1")],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "out\nerr\n");
    assert!(out.stderr.is_empty(), "stderr should be empty");
}

#[test]
fn missing_binary_exits_127() {
    let out = run("/nonexistent/ffmpeg", &[], &[]);
    assert_eq!(out.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot run"));
}

#[test]
fn probe_name_selects_ffprobe() {
    // A symlink named *_probe must target ffprobe, not ffmpeg.
    let dir = std::env::temp_dir().join("rrffmpeg-test-probe");
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("rrffmpeg-probe");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rrffmpeg"), &link).unwrap();

    // PATH shim: "ffprobe" and "ffmpeg" print which one ran.
    let shim = dir.join("shim");
    std::fs::create_dir_all(&shim).unwrap();
    for name in ["ffmpeg", "ffprobe"] {
        let p = shim.join(name);
        std::fs::write(&p, format!("#!/bin/sh\necho {name}\n")).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&p, perm).unwrap();
    }

    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(&link).env("PATH", &path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ffprobe\n");

    let out = Command::new(Path::new(env!("CARGO_BIN_EXE_rrffmpeg")))
        .env("PATH", path)
        .output();
    assert_eq!(String::from_utf8_lossy(&out.unwrap().stdout), "ffmpeg\n");
}

#[test]
fn version_reports_the_build_before_the_config_loads() {
    // A config path that does not parse must not stand between the operator and
    // the answer "which build is this".
    let out = run(
        "/nonexistent/ffmpeg",
        &["--version"],
        &[("RRFFMPEG_CONFIG", "/nonexistent/rrffmpeg.toml")],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("rrffmpeg {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn the_ffmpeg_name_still_forwards_version() {
    // Jellyfin derives ffprobe from the ffmpeg path and probes with -version, so
    // the symlinked name must stay transparent to both spellings.
    let dir = std::env::temp_dir().join("rrffmpeg-test-version-name");
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("ffmpeg");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rrffmpeg"), &link).unwrap();

    // A stand-in that prints its own argument, so the test sees what arrived.
    let real = dir.join("real-ffmpeg");
    std::fs::write(&real, "#!/bin/sh\nprintf '%s\\n' \"$1\"\n").unwrap();
    let mut perm = std::fs::metadata(&real).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    std::fs::set_permissions(&real, perm).unwrap();

    let out = Command::new(&link)
        .arg("--version")
        .env("RRFFMPEG_BIN", &real)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "--version");
}
