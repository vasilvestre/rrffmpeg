//! A job end to end through the agent transport, on loopback.
//!
//! The client is the real binary, the daemon is the real library, and the
//! ffmpeg is a shell script that records what reached the worker. That is
//! enough to prove path mapping, stream separation, the exit code and the
//! stdin bytes Jellyfin writes without a newline.

use rrffmpeg::agent::{generate_cert, serve_on, server_config};
use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const TOKEN: &str = "e2e-token";

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-e2e-{}-{tag}", std::process::id()));
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
fn a_job_crosses_the_agent_with_mapping_and_streams() {
    let root = dir("mapping");
    let cert_dir = root.join("cert");
    generate_cert(&cert_dir, &["127.0.0.1".to_string()]).expect("cert");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let tls = server_config(&cert_dir.join("cert.pem"), &cert_dir.join("key.pem")).expect("tls");
    std::thread::spawn(move || {
        let _ = serve_on(listener, TOKEN.to_string(), tls);
    });

    let clientin = root.join("clientin");
    let workerin = root.join("workerin");
    let clientout = root.join("clientout");
    let workerout = root.join("workerout");
    for d in [&clientin, &workerin, &clientout, &workerout] {
        fs::create_dir_all(d).expect("mapped root");
    }

    // The worker side ffmpeg: record argv, drain stdin, split the streams and
    // leave with a code no other path would produce.
    let args_file = root.join("args");
    let stdin_file = root.join("stdin");
    let fake = root.join("fake-ffmpeg");
    executable(
        &fake,
        &format!(
            "#!/bin/sh\n{{ for a in \"$@\"; do printf '%s\\n' \"$a\"; done; }} > '{args}'\n\
             cat > '{stdin}'\nprintf out-line\nprintf err-line >&2\nexit 7\n",
            args = args_file.display(),
            stdin = stdin_file.display(),
        ),
    );

    let state = root.join("state");
    let config = root.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"agent_token = "{TOKEN}"
agent_cert = "{cert}"
state_dir = "{state}"
queue_wait_secs = 5
ssh_persist_secs = 0

[[hosts]]
name = "worker"
ffmpeg_bin = "{ffmpeg}"
ffprobe_bin = "{ffmpeg}"
agent_addr = "{addr}"

[[hosts.paths]]
from = "{clientin}"
to = "{workerin}"

[[hosts.paths]]
from = "{clientout}"
to = "{workerout}"
"#,
            cert = cert_dir.join("cert.pem").display(),
            state = state.display(),
            ffmpeg = fake.display(),
            clientin = clientin.display(),
            workerin = workerin.display(),
            clientout = clientout.display(),
            workerout = workerout.display(),
        ),
    )
    .expect("config");

    let mut child = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .arg("-i")
        .arg(format!("file:{}/film.mkv", clientin.display()))
        .arg("-f")
        .arg("hls")
        .arg(format!("{}/out.m3u8", clientout.display()))
        .env("RRFFMPEG_CONFIG", &config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rrffmpeg starts");
    // Jellyfin writes q, p, u, c with no newline and keeps the pipe open.
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"qpuc")
        .expect("write stdin");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(7), "stderr: {stderr}");

    // Streams stay apart: stdout carries only what the child wrote to stdout.
    assert!(stdout.contains("out-line"), "{stdout}");
    assert!(!stdout.contains("err-line"), "{stdout}");
    assert!(stderr.contains("err-line"), "{stderr}");
    assert!(!stderr.contains("out-line"), "{stderr}");

    // Both the input and the output path arrived under the worker root.
    let args = fs::read_to_string(&args_file).expect("recorded argv");
    assert!(
        args.contains(&format!("file:{}/film.mkv", workerin.display())),
        "{args}"
    );
    assert!(
        args.contains(&format!("{}/out.m3u8", workerout.display())),
        "{args}"
    );
    assert!(
        !args.contains(clientin.to_str().expect("utf-8")),
        "input was not mapped: {args}"
    );
    assert!(
        !args.contains(clientout.to_str().expect("utf-8")),
        "output was not mapped: {args}"
    );

    // The four bytes reached ffmpeg unchanged.
    assert_eq!(fs::read(&stdin_file).expect("stdin file"), b"qpuc");

    let _ = fs::remove_dir_all(&root);
}
