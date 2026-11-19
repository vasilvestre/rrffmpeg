//! A host with `local = true` is the client's own machine. These tests prove
//! it takes part in placement, wires the streams like every other transport,
//! and leaves nothing behind when the dispatcher is killed.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-local-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perm = fs::metadata(path).expect("stat script").permissions();
    perm.set_mode(0o755);
    fs::set_permissions(path, perm).expect("chmod script");
}

/// Writes the two fake tools and the config around them.
fn setup_with(root: &Path, hosts: &str, config_extra: &str, ffmpeg_body: &str) -> PathBuf {
    let local = root.join("local-ffmpeg");
    executable(&local, ffmpeg_body);
    // The ssh side records its argv: a routing bug that sends the job out
    // would otherwise look like a pass.
    executable(
        &root.join("ssh"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            root.join("ssh-calls").display()
        ),
    );
    let config = format!(
        r#"
state_dir = "{state}"
ssh_bin = "{ssh}"
ssh_persist_secs = 0
fallback_local = false
local_ffmpeg = "{local}"
local_ffprobe = "{local}"
{extra}

{hosts}
"#,
        state = root.join("state").display(),
        ssh = root.join("ssh").display(),
        local = local.display(),
        extra = config_extra,
        hosts = hosts,
    );
    let path = root.join("config.toml");
    fs::write(&path, config).expect("write config");
    path
}

/// The local ffmpeg records every real job, never the `-version` probe.
fn setup(root: &Path, hosts: &str) -> PathBuf {
    let body = format!(
        "#!/bin/sh\n[ \"$1\" = \"-version\" ] && exit 0\ntouch '{}'\n",
        root.join("local-ran").display()
    );
    setup_with(root, hosts, "", &body)
}

const LOCAL_HOST: &str = "[[hosts]]\nname = \"uhd630\"\nlocal = true\ncapabilities = [\"vaapi\"]\n";

fn run(config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(args)
        .env("RRFFMPEG_CONFIG", config)
        .env("RRFFMPEG_PROGRAM", "ffmpeg")
        .output()
        .expect("rrffmpeg starts")
}

#[test]
fn a_job_the_local_host_cannot_encode_goes_to_the_worker() {
    let root = dir("route");
    let config = setup(
        &root,
        &format!(
            "[[hosts]]\nname = \"n100\"\ncapabilities = [\"vaapi\", \"vaapi_av1\"]\n\n{LOCAL_HOST}"
        ),
    );

    let out = run(
        &config,
        &["-c:v", "av1_vaapi", "-i", "movie.mkv", "-f", "null", "-"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");

    let ssh = fs::read_to_string(root.join("ssh-calls")).expect("the worker ran the job");
    assert!(ssh.contains("av1_vaapi"), "{ssh}");
    assert!(
        !root.join("local-ran").exists(),
        "the local host took a job it cannot encode"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_job_only_the_local_host_serves_runs_here() {
    let root = dir("local-only");
    let config = setup(
        &root,
        &format!("[[hosts]]\nname = \"n100\"\ncapabilities = [\"nvenc\"]\n\n{LOCAL_HOST}"),
    );

    let out = run(
        &config,
        &["-c:v", "h264_vaapi", "-i", "movie.mkv", "-f", "null", "-"],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(
        root.join("local-ran").exists(),
        "the local host did not run the job"
    );
    assert!(
        !root.join("ssh-calls").exists(),
        "the job was sent to a host that cannot run it"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn the_capability_probe_of_a_local_host_stays_on_this_machine() {
    let root = dir("probe");
    let probe = root.join("probe-ffmpeg");
    // The script prints a token when the encoder exits 0, so accept exactly
    // one case and check that the reported list is that one.
    executable(
        &probe,
        "#!/bin/sh\ncase \"$*\" in *h264_vaapi*) exit 0;; *) exit 1;; esac\n",
    );
    let config = format!(
        r#"
state_dir = "{state}"
ssh_bin = "{ssh}"
ssh_persist_secs = 0
local_ffmpeg = "{probe}"
local_ffprobe = "{probe}"

[[hosts]]
name = "uhd630"
local = true
"#,
        state = root.join("state").display(),
        ssh = root.join("unused-ssh").display(),
        probe = probe.display(),
    );
    let path = root.join("config.toml");
    fs::write(&path, config).expect("write config");

    let out = run(&path, &["capabilities", "uhd630"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# uhd630"), "{stdout}");
    assert!(stdout.contains("\"vaapi\""), "{stdout}");
    assert!(!stdout.contains("\"vaapi_hevc\""), "{stdout}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_local_host_with_a_path_map_is_refused() {
    let root = dir("bad-map");
    let path = root.join("config.toml");
    fs::write(
        &path,
        r#"
[[hosts]]
name = "uhd630"
local = true

[[hosts.paths]]
from = "/home/alice/media"
to = "/home/bob/media"
"#,
    )
    .expect("write config");

    let out = run(&path, &["-version"]);
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("needs no path map"), "{stderr}");

    let _ = fs::remove_dir_all(&root);
}

/// Jellyfin asks for `stderr_as_stdout` when it wants the log on stdout. The
/// ssh transport merges the streams remotely; the local one must relay its
/// stderr into this process's stdout, not leave it on stderr.
#[test]
fn stderr_reaches_stdout_when_the_flag_is_set() {
    let root = dir("stream-merged");
    let body = "#!/bin/sh\necho OUT\necho ERR >&2\n";
    let config = setup_with(&root, LOCAL_HOST, "stderr_as_stdout = true", body);

    let out = run(&config, &["-i", "movie.mkv", "-f", "null", "-"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("OUT") && stdout.contains("ERR"), "{stdout}");
    assert!(!stderr.contains("ERR"), "{stderr}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn stderr_stays_on_stderr_without_the_flag() {
    let root = dir("stream-split");
    let body = "#!/bin/sh\necho OUT\necho ERR >&2\n";
    let config = setup_with(&root, LOCAL_HOST, "", body);

    let out = run(&config, &["-i", "movie.mkv", "-f", "null", "-"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("ERR"), "{stdout}");
    assert!(stderr.contains("ERR"), "{stderr}");

    let _ = fs::remove_dir_all(&root);
}

/// The janitor reaps a job whose pid file is missing. A job that cannot write
/// one must not run at all.
#[test]
fn a_job_that_cannot_write_its_pid_file_is_refused() {
    let root = dir("pid-fail");
    let config = setup(&root, LOCAL_HOST);
    fs::create_dir_all(root.join("state")).expect("create state dir");
    fs::write(root.join("state").join("pid"), "not a directory").expect("block the pid dir");

    let out = run(&config, &["-i", "movie.mkv", "-f", "null", "-"]);
    assert_eq!(out.status.code(), Some(127), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("pid file"), "{stderr}");

    let _ = fs::remove_dir_all(&root);
}

/// Jellyfin cancels a transcode by killing the process it started, which is
/// this one. Local mode exec'd, so the signal reached ffmpeg. Here ffmpeg is a
/// child, and it must not outlive its parent.
#[test]
fn a_killed_dispatcher_takes_its_job_with_it() {
    let root = dir("pdeath");
    let body = format!(
        "#!/bin/sh\necho $$ > '{}'\nsleep 30\n",
        root.join("job.pid").display()
    );
    let config = setup_with(&root, LOCAL_HOST, "", &body);

    let mut dispatcher = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"))
        .args(["-i", "movie.mkv", "-f", "null", "-"])
        .env("RRFFMPEG_CONFIG", &config)
        .env("RRFFMPEG_PROGRAM", "ffmpeg")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("rrffmpeg starts");

    let pid = wait_for_pid(&root.join("job.pid"));
    assert!(pid > 0, "the job never started");

    // SIGKILL, the way a cancel on a stuck job arrives.
    unsafe { libc::kill(dispatcher.id() as i32, libc::SIGKILL) };
    let _ = dispatcher.wait();

    let mut gone = false;
    for _ in 0..200 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !gone {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    let _ = fs::remove_dir_all(&root);
    assert!(gone, "the local job outlived the dispatcher");
}

fn wait_for_pid(path: &Path) -> i32 {
    for _ in 0..300 {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    0
}
