//! A killed client leaves a Started row behind. The janitor must reap it, and
//! the slot it held must be free at once, because the kernel drops the flock
//! with the process.
//!
//! The fake ssh runs the remote command as a child and kills it when the
//! client goes away, the way a real ssh connection does. The janitor therefore
//! sees a dead process while the journal still lists the job.

use rrffmpeg::state::{Event, Record, records};
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Runs the remote command as a child. When its parent (the rrffmpeg client)
/// disappears, it kills the child and exits. `$parent` is captured once, so a
/// reparent to init does not fool the loop.
const FAKE_SSH: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; break; fi
  shift
done
if [ $# -gt 0 ]; then shift; fi
parent=$PPID
remote="$*"
sh -c "$remote" &
worker=$!
while kill -0 "$parent" 2>/dev/null; do
  if ! kill -0 "$worker" 2>/dev/null; then
    wait "$worker"
    exit $?
  fi
  sleep 0.1
done
kill -TERM "$worker" 2>/dev/null
wait "$worker" 2>/dev/null
exit 143
"#;

const WORKER: &str = r#"#!/bin/sh
printf '%s\n' "$$" >> '@LOG@'
exec sleep 60
"#;

static SERIAL: Mutex<()> = Mutex::new(());

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rrffmpeg-crash-{}-{n}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::write(path, body).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
    path.to_path_buf()
}

fn wait_within(limit: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn kill_pid(pid: u32, signal: i32) {
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

fn pids(log: &Path) -> Vec<u32> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn spawn(config: &Path, dir: &Path) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rrffmpeg"));
    cmd.arg("-i")
        .arg("file:/home/alice/media/movie.mkv")
        .env("RRFFMPEG_CONFIG", config)
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env("XDG_STATE_HOME", dir)
        .env_remove("RRFFMPEG_BIN")
        .env_remove("RRFFMPEG_STDERR_AS_STDOUT")
        .env_remove("RRFFMPEG_PROGRAM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("rrffmpeg should start")
}

/// The active set the journal implies: Started adds, Finished and Reaped remove.
fn active(records: &[Record]) -> HashSet<String> {
    let mut jobs: HashSet<String> = HashSet::new();
    for record in records {
        match &record.ev {
            Event::Started { job, .. } => {
                jobs.insert(job.clone());
            }
            Event::Finished { job, .. } | Event::Reaped { job } => {
                jobs.remove(job);
            }
            Event::HostDown { .. } | Event::HostUp { .. } => {}
        }
    }
    jobs
}

#[test]
fn the_janitor_reaps_a_job_killed_with_its_client() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = scratch("reap");
    let state = dir.join("state");
    let log = dir.join("jobs.log");
    let ssh = script(&dir.join("ssh"), FAKE_SSH);
    let worker = script(
        &dir.join("worker-ffmpeg"),
        &WORKER.replace("@LOG@", &log.display().to_string()),
    );
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "state_dir = \"{state}\"\n\
             ssh_bin = \"{ssh}\"\n\
             ssh_persist_secs = 0\n\
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
    )
    .expect("write config");

    // First client: its Started row is on disk before the kill.
    let mut first = spawn(&config, &dir);
    let recorded = wait_within(Duration::from_secs(5), || !pids(&log).is_empty());
    assert!(recorded, "the worker never started");
    let orphan_pid = pids(&log)[0];

    let started = wait_within(Duration::from_secs(5), || {
        records(&state)
            .map(|all| {
                all.iter()
                    .any(|r| matches!(&r.ev, Event::Started { host, .. } if host == "worker"))
            })
            .unwrap_or(false)
    });
    let job = records(&state)
        .expect("read journal")
        .into_iter()
        .find_map(|record| match record.ev {
            Event::Started { job, .. } => Some(job),
            _ => None,
        });
    let job = match (started, job) {
        (true, Some(job)) => job,
        _ => {
            let _ = first.kill();
            let _ = first.wait();
            kill_pid(orphan_pid, libc::SIGKILL);
            let _ = std::fs::remove_dir_all(&dir);
            panic!("the killed job never reached the journal");
        }
    };

    first.kill().expect("kill the first client");
    let _ = first.wait();
    assert!(
        wait_within(Duration::from_secs(5), || !pid_alive(orphan_pid)),
        "the worker outlived its client, so the orphan is not an orphan"
    );

    // Second client: the slot the dead client held is free at once.
    let began = Instant::now();
    let mut second = spawn(&config, &dir);
    let moved_on = wait_within(Duration::from_secs(5), || pids(&log).len() >= 2);
    let second_pid = pids(&log).into_iter().find(|pid| *pid != orphan_pid);
    let slot_free = moved_on.then(|| began.elapsed());

    // The janitor sleeps 10 s, then needs two cycles to judge the job dead.
    let reaped = wait_within(Duration::from_secs(30), || {
        records(&state)
            .map(|all| {
                all.iter()
                    .any(|r| matches!(&r.ev, Event::Reaped { job: id } if id == &job))
            })
            .unwrap_or(false)
    });
    let reap_took = began.elapsed();

    // Stop the second job and every process it left behind.
    second.kill().expect("kill the second client");
    let _ = second.wait();
    if let Some(pid) = second_pid {
        kill_pid(pid, libc::SIGTERM);
        kill_pid(pid, libc::SIGKILL);
    }
    kill_pid(orphan_pid, libc::SIGKILL);

    let all = records(&state).expect("final journal");
    let started_rows = all
        .iter()
        .filter(|r| matches!(&r.ev, Event::Started { job: id, .. } if id == &job))
        .count();
    let reaped_rows = all
        .iter()
        .filter(|r| matches!(&r.ev, Event::Reaped { job: id } if id == &job))
        .count();
    let still_active = active(&all).contains(&job);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        slot_free.is_some(),
        "the next job did not take the freed slot: {slot_free:?}"
    );
    assert!(
        reaped,
        "the janitor never reaped the killed job within {reap_took:?}"
    );
    assert_eq!(started_rows, 1, "the killed job lost its Started row");
    assert_eq!(reaped_rows, 1, "the killed job must be reaped exactly once");
    assert!(!still_active, "the reaped job is still counted as active");
}
