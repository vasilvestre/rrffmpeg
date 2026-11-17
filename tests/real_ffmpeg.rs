//! Checks against the real ffmpeg. Skipped when ffmpeg is not installed.
//! These are the tests that matter: a shim proves wiring, not fidelity.

use std::process::Command;
use std::time::Instant;

fn have(bin: &str) -> bool {
    Command::new(bin).arg("-version").output().is_ok()
}

fn rrffmpeg(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(args)
        .output()
        .expect("rrffmpeg should start")
}

/// The deployment form: a symlink whose name contains `probe`.
fn probe_link() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("rrffmpeg-real");
    std::fs::create_dir_all(&dir).unwrap();
    let link = dir.join("rrffmpeg-probe");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_rrffmpeg"), &link).unwrap();
    link
}

#[test]
fn real_ffprobe_output_matches_direct_call() {
    if !have("ffprobe") {
        return;
    }
    let dir = std::env::temp_dir().join("rrffmpeg-real");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.nut");
    let made = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=128x96:rate=5",
            "-t",
            "0.6",
        ])
        .arg(&src)
        .status()
        .unwrap();
    assert!(made.success(), "fixture generation failed");

    let args = [
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
    ];
    let direct = Command::new("ffprobe")
        .args(args)
        .arg(&src)
        .output()
        .unwrap();
    let via_rr = Command::new(probe_link())
        .args(args)
        .arg(&src)
        .output()
        .unwrap();

    assert!(direct.status.success());
    assert_eq!(
        String::from_utf8_lossy(&via_rr.stdout),
        String::from_utf8_lossy(&direct.stdout)
    );
    assert_eq!(via_rr.status.code(), direct.status.code());
}

#[test]
fn real_ffmpeg_encode_matches_direct_call() {
    if !have("ffmpeg") {
        return;
    }
    let dir = std::env::temp_dir().join("rrffmpeg-real");
    std::fs::create_dir_all(&dir).unwrap();
    let out_a = dir.join("a.mp4");
    let out_b = dir.join("b.mp4");
    let base = [
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=128x96:rate=5",
        "-t",
        "0.6",
        "-c:v",
        "mpeg4",
        "-fflags",
        "+bitexact",
        "-flags:v",
        "+bitexact",
    ];

    let direct = Command::new("ffmpeg")
        .args(base)
        .arg(&out_a)
        .status()
        .unwrap();
    let via_rr = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(base)
        .arg(&out_b)
        .status()
        .unwrap();

    assert!(direct.success() && via_rr.success());
    assert_eq!(
        std::fs::read(&out_a).unwrap(),
        std::fs::read(&out_b).unwrap(),
        "encoded bytes must be identical"
    );
}

#[test]
fn dispatch_overhead_stays_under_50ms() {
    if !have("ffmpeg") {
        return;
    }
    // Warm the page cache so we time the dispatcher, not the disk.
    let _ = rrffmpeg(&["-version"]);
    let mut worst = std::time::Duration::ZERO;
    for _ in 0..20 {
        let t = Instant::now();
        let out = rrffmpeg(&["-version"]);
        let took = t.elapsed();
        assert!(out.status.success());
        worst = worst.max(took);
    }
    assert!(worst.as_millis() < 50, "worst dispatch took {worst:?}");
}

#[test]
fn stderr_as_stdout_works_with_real_ffmpeg() {
    if !have("ffmpeg") {
        return;
    }
    // ffmpeg -version logs to stderr; the flag must move it to stdout.
    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .arg("-version")
        .env("RRFFMPEG_STDERR_AS_STDOUT", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("ffmpeg version"));
    assert!(out.stderr.is_empty(), "stderr should be empty");
}
