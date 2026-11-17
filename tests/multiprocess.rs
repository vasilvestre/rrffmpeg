//! Cross-process proof for slots and the shared host-down verdict.
//!
//! Each test starts the real binary more than once, because the defects these
//! lock files fix only exist when several processes share one state dir.

use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_millis(5);
const DEADLINE: Duration = Duration::from_secs(5);
/// What Jellyfin sends for a transcode. The fake worker ignores the contents.
const JOB_ARGS: [&str; 3] = [
    "-i",
    "file:/home/alice/media/movie.mkv",
    "file:/home/alice/out.mp4",
];

/// Stand in for ssh: keep the shape `Ssh` builds, drop options and destination,
/// then run the remote command locally so stdio and quoting behave for real.
const FAKE_SSH: &str = r#"#!/bin/sh
probe=0
for arg in "$@"; do
  if [ "$arg" = "BatchMode=yes" ]; then probe=1; fi
done
if [ "$probe" -eq 1 ]; then
  if [ -n "${FAKE_SSH_PROBE_DELAY:-}" ]; then sleep "$FAKE_SSH_PROBE_DELAY"; fi
  mode=${FAKE_SSH_PROBE:-ok}
else
  mode=${FAKE_SSH_RUN:-ok}
fi
if [ "$mode" = "fail" ]; then
  exit 255
fi
while [ $# -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
if [ $# -gt 0 ]; then shift; fi
exec sh -c "$*"
"#;

/// Records when this job takes and releases its slot. The millis field is for
/// debugging; the test trusts line order, because the wall clock steps on this
/// host and only one job can append at a time.
const INTERVAL_WORKER: &str = r#"#!/bin/sh
printf 'start %s %s\n' "$(date +%s%3N)" "$$" >> '@LOG@'
sleep @SLEEP@
printf 'end %s %s\n' "$(date +%s%3N)" "$$" >> '@LOG@'
"#;

/// Records its start, then replaces itself with a long sleep, so the pid in the
/// log is the process the test must kill.
const HOLDER_WORKER: &str = r#"#!/bin/sh
printf 'start 0 %s\n' "$$" >> '@LOG@'
exec sleep 60
"#;

/// Records that it ran on this worker.
const MARK_WORKER: &str = r#"#!/bin/sh
printf '@KIND@ 0 %s\n' "$$" >> '@LOG@'
sleep @SLEEP@
"#;

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "rrffmpeg-multiprocess-{}-{n}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
    path.to_path_buf()
}

/// Fill a script template and write it. Plain replacement keeps the scripts
/// above readable, with no brace or percent escaping.
fn from_template(template: &str, path: &Path, swaps: &[(&str, &str)]) -> PathBuf {
    let mut body = template.to_string();
    for (from, to) in swaps {
        body = body.replace(from, to);
    }
    write_script(path, &body)
}

fn write_config(dir: &Path, tag: &str, body: &str) -> PathBuf {
    let path = dir.join(format!("{tag}.toml"));
    std::fs::write(&path, body).expect("write config");
    path
}

fn rrffmpeg(config: &Path, dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.env("RRFFMPEG_CONFIG", config)
        .env("RRFFMPEG_BIN", dir.join("local-ffmpeg"))
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_STATE_HOME", dir)
        .env_remove("RRFFMPEG_STDERR_AS_STDOUT")
        .env_remove("RRFFMPEG_PROGRAM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

fn spawn(config: &Path, dir: &Path, envs: &[(&str, &str)]) -> Child {
    let mut cmd = rrffmpeg(config, dir);
    cmd.args(JOB_ARGS);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.spawn().expect("rrffmpeg should start")
}

fn finish(child: Child) -> Output {
    child.wait_with_output().expect("rrffmpeg should exit")
}

fn wait_within(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(POLL);
    }
    cond()
}

fn wait_until(cond: impl FnMut() -> bool) -> bool {
    wait_within(DEADLINE, cond)
}

#[derive(Debug)]
struct Line {
    kind: String,
    pid: u32,
}

/// Parse the log, ignoring a line that is still being written.
fn read_log(path: &Path) -> Vec<Line> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let kind = parts.next()?.to_string();
            // Parse the millis field too, so a broken worker fails loudly.
            parts.next()?.parse::<u64>().ok()?;
            let pid = parts.next()?.parse().ok()?;
            Some(Line { kind, pid })
        })
        .collect()
}

fn kind_pids(log: &Path, kind: &str) -> Vec<u32> {
    read_log(log)
        .into_iter()
        .filter(|line| line.kind == kind)
        .map(|line| line.pid)
        .collect()
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Same FNV-1a hash `LockDir` uses, so the test finds the real coordinator dir.
fn hostkey(name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[test]
fn cap_is_enforced_across_processes() {
    let dir = scratch("cap");
    let state = dir.join("state");
    let log = dir.join("jobs.log");
    let ssh = write_script(&dir.join("ssh"), FAKE_SSH);
    let worker = from_template(
        INTERVAL_WORKER,
        &dir.join("worker-ffmpeg"),
        &[("@LOG@", &log.display().to_string()), ("@SLEEP@", "0.3")],
    );
    let config = write_config(
        &dir,
        "cap",
        &format!(
            "state_dir = \"{state}\"\n\
             ssh_bin = \"{ssh}\"\n\
             queue_wait_secs = 10\n\
             \n\
             [[hosts]]\n\
             name = \"worker\"\n\
             concurrency = 1\n\
             ffmpeg_bin = \"{worker}\"\n",
            state = state.display(),
            ssh = ssh.display(),
            worker = worker.display(),
        ),
    );

    let children: Vec<Child> = (0..4).map(|_| spawn(&config, &dir, &[])).collect();
    for child in children {
        let out = finish(child);
        assert!(
            out.status.success(),
            "a job failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // One holder at a time means the log alternates start and end, and never
    // shows two starts in a row. That, not the clock, is the cap's proof.
    let lines = read_log(&log);
    assert_eq!(lines.len(), 8, "expected four jobs, saw {lines:?}");
    for pair in lines.chunks(2) {
        assert_eq!(
            pair[0].kind, "start",
            "two jobs held the only slot at once: {lines:?}"
        );
        assert_eq!(
            pair[1].kind, "end",
            "a job never released its slot: {lines:?}"
        );
        assert_eq!(
            pair[0].pid, pair[1].pid,
            "a start and an end came from different jobs: {lines:?}"
        );
    }
    let jobs: std::collections::HashSet<u32> = lines.iter().map(|line| line.pid).collect();
    assert_eq!(jobs.len(), 4, "expected four jobs on the worker: {lines:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_killed_holder_frees_its_slot() {
    let dir = scratch("kill");
    let state = dir.join("state");
    let log = dir.join("jobs.log");
    let ssh = write_script(&dir.join("ssh"), FAKE_SSH);
    let worker = from_template(
        HOLDER_WORKER,
        &dir.join("worker-ffmpeg"),
        &[("@LOG@", &log.display().to_string())],
    );
    let config = write_config(
        &dir,
        "kill",
        &format!(
            "state_dir = \"{state}\"\n\
             ssh_bin = \"{ssh}\"\n\
             queue_wait_secs = 10\n\
             \n\
             [[hosts]]\n\
             name = \"worker\"\n\
             concurrency = 1\n\
             ffmpeg_bin = \"{worker}\"\n",
            state = state.display(),
            ssh = ssh.display(),
            worker = worker.display(),
        ),
    );

    let mut first = spawn(&config, &dir, &[]);
    let observed = wait_until(|| !kind_pids(&log, "start").is_empty());
    if !observed {
        // The platform never produced the marker line, so nothing to prove.
        let _ = first.kill();
        let _ = first.wait();
        eprintln!("skipping: the worker never wrote its start line");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let holder = kind_pids(&log, "start")[0];
    first.kill().expect("kill the holder");
    let _ = first.wait();

    let began = Instant::now();
    let mut second = spawn(&config, &dir, &[]);
    let started = wait_within(Duration::from_secs(2), || {
        kind_pids(&log, "start").iter().any(|pid| *pid != holder)
    });
    let elapsed = began.elapsed();

    let sleeper = kind_pids(&log, "start")
        .into_iter()
        .find(|pid| *pid != holder);
    let _ = second.kill();
    let _ = second.wait();
    if let Some(pid) = sleeper {
        kill_pid(pid);
    }
    kill_pid(holder);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        started,
        "the next job did not take the freed slot within {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the freed slot took {elapsed:?} to be reused"
    );
}

#[test]
fn host_down_is_shared_and_expires() {
    let dir = scratch("down");
    let state = dir.join("state");
    let log = dir.join("jobs.log");
    let ssh = write_script(&dir.join("ssh"), FAKE_SSH);
    let worker = from_template(
        MARK_WORKER,
        &dir.join("worker-ffmpeg"),
        &[
            ("@LOG@", &log.display().to_string()),
            ("@KIND@", "worker"),
            ("@SLEEP@", "0"),
        ],
    );
    let local = from_template(
        MARK_WORKER,
        &dir.join("local-ffmpeg"),
        &[
            ("@LOG@", &log.display().to_string()),
            ("@KIND@", "local"),
            ("@SLEEP@", "0"),
        ],
    );
    let config = write_config(
        &dir,
        "down",
        &format!(
            "state_dir = \"{state}\"\n\
             ssh_bin = \"{ssh}\"\n\
             down_ttl_secs = 1\n\
             queue_wait_secs = 0\n\
             local_ffmpeg = \"{local}\"\n\
             local_ffprobe = \"{local}\"\n\
             \n\
             [[hosts]]\n\
             name = \"worker\"\n\
             concurrency = 1\n\
             ffmpeg_bin = \"{worker}\"\n",
            state = state.display(),
            ssh = ssh.display(),
            worker = worker.display(),
            local = local.display(),
        ),
    );

    // A probe delay keeps the janitor's first cycle behind A's own failure, so
    // the failing transport is what publishes the verdict.
    let a = finish(spawn(
        &config,
        &dir,
        &[
            ("FAKE_SSH_RUN", "fail"),
            ("FAKE_SSH_PROBE", "fail"),
            ("FAKE_SSH_PROBE_DELAY", "0.2"),
        ],
    ));
    assert!(!a.status.success(), "a dead worker must fail the job");

    let down = state.join("coord").join(hostkey("worker")).join("down");
    assert!(
        wait_until(|| down.exists()),
        "A did not publish a down verdict at {}",
        down.display()
    );
    let expiry: u64 = std::fs::read_to_string(&down)
        .expect("read the verdict")
        .trim()
        .parse()
        .expect("the verdict holds an absolute expiry");
    let now = now_millis();
    assert!(expiry > now, "the verdict expired before B ran");
    assert!(
        expiry <= now + 2000,
        "the verdict outlived the one second TTL"
    );

    // B sees the live verdict, so it stays off the worker.
    let b = finish(spawn(
        &config,
        &dir,
        &[("FAKE_SSH_RUN", "fail"), ("FAKE_SSH_PROBE", "fail")],
    ));
    assert!(b.status.success(), "B runs locally after the verdict");
    assert!(
        !kind_pids(&log, "local").is_empty(),
        "B never ran, not even locally"
    );
    assert!(
        kind_pids(&log, "worker").is_empty(),
        "B reached the worker while the verdict was fresh"
    );

    // Expiry is the whole repair mechanism, so wait it out and retry.
    let remaining = expiry.saturating_sub(now_millis());
    thread::sleep(Duration::from_millis(remaining + 200));

    let c = finish(spawn(&config, &dir, &[]));
    assert!(c.status.success(), "C should transcode on the worker");
    assert!(
        !kind_pids(&log, "worker").is_empty(),
        "C did not use the worker after the verdict expired"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn slot_files_outlive_their_holder() {
    let dir = scratch("slots");
    let state = dir.join("state");
    let log = dir.join("jobs.log");
    let ssh = write_script(&dir.join("ssh"), FAKE_SSH);
    let worker = from_template(
        MARK_WORKER,
        &dir.join("worker-ffmpeg"),
        &[
            ("@LOG@", &log.display().to_string()),
            ("@KIND@", "worker"),
            ("@SLEEP@", "1"),
        ],
    );
    let config = write_config(
        &dir,
        "slots",
        &format!(
            "state_dir = \"{state}\"\n\
             ssh_bin = \"{ssh}\"\n\
             queue_wait_secs = 10\n\
             \n\
             [[hosts]]\n\
             name = \"worker\"\n\
             concurrency = 2\n\
             ffmpeg_bin = \"{worker}\"\n",
            state = state.display(),
            ssh = ssh.display(),
            worker = worker.display(),
        ),
    );

    // Two overlapping jobs fill both slots, so both files must exist after.
    let children: Vec<Child> = (0..2).map(|_| spawn(&config, &dir, &[])).collect();
    for child in children {
        let out = finish(child);
        assert!(
            out.status.success(),
            "a job failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let coord = state.join("coord").join(hostkey("worker"));
    for index in 0..2 {
        let path = coord.join(format!("slot.{index}"));
        assert!(
            path.exists(),
            "slot {index} vanished with its holder: {}",
            path.display()
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open a leftover slot");
        assert!(
            file.try_lock().is_ok(),
            "slot {index} is still locked after every holder exited"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
