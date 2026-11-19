//! `rrffmpeg-agent` argument handling: the daemon itself is tested in
//! `src/agent.rs` over a real socket, so these tests only cover startup.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-agent-cli-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn agent(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rrffmpeg-agent"))
        .args(args)
        .env_remove("RRFFMPEG_AGENT_TOKEN")
        .output()
        .expect("rrffmpeg-agent starts")
}

#[test]
fn help_prints_the_usage_and_stops() {
    let out = agent(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("usage: rrffmpeg-agent"), "{stdout}");
}

#[test]
fn version_prints_the_package_version() {
    let out = agent(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "missing version in {stdout}"
    );
}

#[test]
fn an_unknown_option_is_refused() {
    let out = agent(&["--nope"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown option --nope"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_missing_token_is_refused_before_the_certificate() {
    let out = agent(&["--cert", "/tmp/cert.pem", "--key", "/tmp/key.pem"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("a token is required"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_missing_certificate_or_key_is_refused() {
    let out = agent(&["--token", "secret", "--cert", "/tmp/cert.pem"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--cert and --key are required"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_unreadable_certificate_stops_startup() {
    let root = dir("bad-cert");
    let out = agent(&[
        "--token",
        "secret",
        "--listen",
        "127.0.0.1:0",
        "--cert",
        root.join("missing.pem").to_str().unwrap(),
        "--key",
        root.join("missing.key").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!out.stderr.is_empty(), "an error must be reported");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_without_a_san_fails() {
    let root = dir("no-san");
    let out = agent(&["--init", root.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("at least one name"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn init_writes_a_certificate_and_a_key() {
    let root = dir("init");
    let out = agent(&[
        "--init",
        root.to_str().unwrap(),
        "--san",
        "localhost",
        "--san",
        "127.0.0.1",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("wrote"), "{stdout}");
    for name in ["cert.pem", "key.pem"] {
        let path = root.join(name);
        let len = fs::metadata(&path).expect("written file").len();
        assert!(len > 0, "{name} is empty");
    }
    let _ = fs::remove_dir_all(&root);
}
