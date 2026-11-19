//! How `RRFFMPEG_CONFIG` picks the file, and what a bad or missing one does.
//!
//! The local stand-in is `/bin/true`, so the exit code alone says whether the
//! dispatcher took the configured path or fell back to the defaults.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-env-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Run the dispatcher with the environment a Jellyfin call would give it,
/// minus any config the developer's own home may hold.
fn run(config: Option<&Path>, xdg: &Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.args(["-i", "file:/m/x.mkv", "-c:v", "libx264"])
        .env("RRFFMPEG_BIN", "/bin/true")
        .env("XDG_CONFIG_HOME", xdg)
        .env_remove("RRFFMPEG_PROGRAM")
        .env_remove("RRFFMPEG_CONFIG")
        .stdin(Stdio::null());
    if let Some(path) = config {
        cmd.env("RRFFMPEG_CONFIG", path);
    }
    cmd.output().expect("rrffmpeg starts")
}

#[test]
fn a_config_path_is_read_and_a_bad_or_missing_one_is_handled() {
    let root = dir("select");
    let xdg = root.join("xdg");
    fs::create_dir_all(&xdg).expect("xdg dir");

    // The file sets fallback_local = false with no worker. Only reading it
    // turns the transcode into a refusal: the default would run /bin/true.
    let chosen = root.join("chosen.toml");
    fs::write(&chosen, "fallback_local = false\n").expect("config");
    let out = run(Some(&chosen), &xdg);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("fallback_local"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Invalid TOML: a config error, exit 127, and a message on stderr.
    let broken = root.join("broken.toml");
    fs::write(&broken, "hosts = \"not a list\"\n").expect("broken config");
    let out = run(Some(&broken), &xdg);
    assert_eq!(out.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config"), "{stderr}");

    // A missing file is not an error: it means no workers, so the default
    // fallback runs the local binary and the exit code is zero.
    let missing = root.join("absent.toml");
    let out = run(Some(&missing), &xdg);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // With no RRFFMPEG_CONFIG at all, an empty XDG tree gives the defaults.
    let out = run(None, &xdg);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = fs::remove_dir_all(&root);
}
