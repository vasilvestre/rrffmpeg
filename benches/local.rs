//! What a job costs when it runs on the client's own GPU.
//!
//! The local transport is the only one with no network, so its cost is the
//! dispatcher's own work: open the journal, take a slot, spawn, wait, close the
//! journal. Nothing here includes ffmpeg's own startup: `/bin/true` stands in.

#[path = "harness/mod.rs"]
mod harness;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rrffmpeg::state::{Event, Journal};
use rrffmpeg::transport::{Local, Router, Transport};
use rrffmpeg::{Job, Program};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

fn job() -> Job {
    Job {
        id: "bench".into(),
        program: Program::Ffmpeg,
        args: vec!["-version".into()],
        priority: 0,
        capabilities: Vec::new(),
    }
}

/// One job through the local transport: spawn, wait, reap the pid file.
fn local_run(c: &mut Criterion) {
    let dir = harness::temp_dir("local-run");
    let local = Local::new(
        PathBuf::from("/bin/true"),
        PathBuf::from("/bin/true"),
        dir.join("pid"),
        BTreeMap::new(),
    )
    .expect("local transport");
    let host = rrffmpeg::Host {
        local: true,
        ..harness::host("client", 1)
    };
    let router = Router {
        ssh: rrffmpeg::transport::Ssh::new(PathBuf::from("ssh")),
        agent: None,
        local: Some(local),
    };
    let job = job();
    c.bench_function("local_spawn", |b| b.iter(|| router.run(&host, &job, false)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A process that dispatches once pays one open and one close per run, and the
/// close lasts until the journal is fsynced.
fn journal_open_close(c: &mut Criterion) {
    c.bench_function("journal_open_close", |b| {
        b.iter_batched(
            || harness::temp_dir("journal-open"),
            |dir| {
                let journal = Journal::open(&dir).expect("open journal");
                drop(journal);
                dir
            },
            BatchSize::PerIteration,
        )
    });
}

/// The same, with the two events a finished job writes.
fn journal_open_write_close(c: &mut Criterion) {
    c.bench_function("journal_open_write_close", |b| {
        b.iter_batched(
            || harness::temp_dir("journal-write"),
            |dir| {
                let journal = Journal::open(&dir).expect("open journal");
                journal.append(Event::Started {
                    job: "bench".into(),
                    host: "client".into(),
                    elapsed_us: 0,
                    program: "ffmpeg".into(),
                    in_flight: 1,
                });
                journal.append(Event::Finished {
                    job: "bench".into(),
                    code: 0,
                    host: "client".into(),
                    dur_us: 1,
                    stats: Box::default(),
                });
                drop(journal);
                dir
            },
            BatchSize::PerIteration,
        )
    });
}

/// The startup floor of the harness itself, so a reader can subtract it.
fn spawn_floor(c: &mut Criterion) {
    c.bench_function("spawn_true", |b| {
        b.iter(|| {
            std::process::Command::new("/bin/true")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        })
    });
}

criterion_group!(
    local,
    local_run,
    journal_open_close,
    journal_open_write_close,
    spawn_floor
);
criterion_main!(local);
