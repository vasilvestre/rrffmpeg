//! `rrffmpeg capabilities <host>` prints a line you can paste into a config.
//!
//! The ssh is a script that prints tokens plus noise, the way a real worker
//! prints encoder lines and unrelated chatter.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-caps-{}-{tag}", std::process::id()));
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

#[test]
fn capabilities_prints_a_line_the_config_accepts() {
    let root = dir("tokens");
    let ssh = root.join("ssh");
    executable(
        &ssh,
        "#!/bin/sh\nprintf '%s\\n' encoder-lines : vaapi vaapi_hevc qsv banana\n",
    );

    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            "ssh_bin = \"{}\"\nssh_persist_secs = 0\n\n[[hosts]]\nname = \"worker\"\nffmpeg_bin = \"ffmpeg\"\n",
            ssh.display()
        ),
    )
    .expect("config");

    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(["capabilities", "worker"])
        .env("RRFFMPEG_CONFIG", &config)
        .output()
        .expect("rrffmpeg starts");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# worker"), "{stdout}");
    for token in ["vaapi", "vaapi_hevc", "qsv"] {
        assert!(
            stdout.contains(&format!("\"{token}\"")),
            "{token} missing from {stdout}"
        );
    }
    // Only known tokens are reported, never a line of noise.
    assert!(!stdout.contains("banana"), "{stdout}");

    // Paste the printed line into the example config: it must still parse,
    // under a fresh host, back into the same token list.
    let line = stdout
        .lines()
        .find(|line| line.starts_with("capabilities = "))
        .expect("a capabilities line");
    let snippet = format!("[[hosts]]\nname = \"probed\"\n{line}\n");
    let example =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml"))
            .expect("example config");
    let parsed = rrffmpeg::config::Config::from_toml(&format!("{example}\n{snippet}"))
        .expect("the printed line is valid config");
    let host = parsed
        .hosts
        .iter()
        .find(|host| host.name == "probed")
        .expect("the pasted host is in the parsed config");
    assert_eq!(host.capabilities, ["vaapi", "vaapi_hevc", "qsv"]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_failed_probe_still_reports_each_host_and_exits_nonzero() {
    let root = dir("failed");
    let ssh = root.join("ssh");
    executable(&ssh, "#!/bin/sh\necho 'no route to host' >&2\nexit 255\n");

    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            "ssh_bin = \"{}\"\nssh_persist_secs = 0\n\n[[hosts]]\nname = \"worker\"\n",
            ssh.display()
        ),
    )
    .expect("config");

    let out = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(["capabilities", "worker"])
        .env("RRFFMPEG_CONFIG", &config)
        .output()
        .expect("rrffmpeg starts");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("capability probe on worker failed"), "{stderr}");
    let _ = fs::remove_dir_all(&root);
}
