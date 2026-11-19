//! Journal cost: the queued append, rotation at the size cap, and replay.
//!
//! Append must stay off the fsync path. Rotation and replay are the bulk
//! costs, and they belong to the flusher and the janitor, not the dispatch.

#[path = "harness/mod.rs"]
mod harness;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use rrffmpeg::state::{Event, Journal};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn started(job: &str) -> Event {
    Event::Started {
        job: job.into(),
        host: "worker".into(),
        elapsed_us: 0,
        program: "ffmpeg".into(),
        in_flight: 1,
    }
}

/// One append per iteration, no fsync: the dispatch-path write.
fn append(c: &mut Criterion) {
    let dir = harness::temp_dir("journal-append");
    let journal = Journal::open(&dir).expect("open journal");
    c.bench_function("journal_append", |b| {
        b.iter(|| journal.append(started("job")));
    });
    drop(journal);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Removes the bench dir when criterion drops the routine output, outside the
/// timed region.
struct RemoveDir(PathBuf);

impl Drop for RemoveDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One iteration is one full rotation: append until the 8 MiB cap renames the
/// file to `journal.1`.
fn rotation(c: &mut Criterion) {
    c.bench_function("journal_rotation", |b| {
        b.iter_batched(
            || {
                let dir = harness::temp_dir("journal-rotate");
                let journal = Journal::open(&dir).expect("open journal");
                (RemoveDir(dir), journal)
            },
            |(dir, journal)| {
                let mut appends = 0u64;
                loop {
                    journal.append(started("job"));
                    appends += 1;
                    // Check the rotated file outside the timed append, every 256
                    // records, so the stat does not pollute the samples.
                    if appends.is_multiple_of(256) {
                        if Path::new(&dir.0).join("journal.1").exists() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    assert!(appends < 1_000_000, "journal never rotated at the cap");
                }
                drop(journal);
                dir
            },
            BatchSize::PerIteration,
        );
    });
}

/// Replay a journal of many records through the janitor's read.
fn replay(c: &mut Criterion) {
    const RECORDS: usize = 5_000;
    let dir = harness::temp_dir("journal-replay");
    let journal = Journal::open(&dir).expect("open journal");
    for i in 0..RECORDS {
        journal.append(started(&format!("job-{i}")));
    }
    drop(journal);

    c.bench_function("journal_replay_5000", |b| {
        b.iter(|| {
            let journal = Journal::open(&dir).expect("reopen journal");
            std::hint::black_box(journal.active_on("worker"))
        });
    });

    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group! {
    name = append_benches;
    config = Criterion::default()
        .sample_size(50)
        .measurement_time(Duration::from_secs(3));
    targets = append
}
criterion_group! {
    name = rotation_benches;
    config = Criterion::default().sample_size(10);
    targets = rotation
}
criterion_group! {
    name = replay_benches;
    config = Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(5));
    targets = replay
}
criterion_main!(append_benches, rotation_benches, replay_benches);
