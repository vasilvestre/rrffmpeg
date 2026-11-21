//! Queue cost: scan a busy lane, and the enqueue/drop of one ticket.

#[path = "harness/mod.rs"]
mod harness;

use criterion::{Criterion, criterion_group, criterion_main};
use rrffmpeg::queue::{Lane, Queue};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Number of tickets a busy lane holds in the scan bench.
const TICKETS: u64 = 10_000;

/// A queue with one lane of `TICKETS` tickets, as a busy lane looks on disk.
fn busy_lane(tag: &str) -> (PathBuf, Queue, Lane) {
    let base = harness::temp_dir(tag);
    let coord = base.join("coord");
    std::fs::create_dir_all(&coord).expect("create coord dir");
    let queue = Queue::new(&coord);

    // One real ticket creates the lane directory. Read its path back from the
    // public coord root, so no private helper is needed.
    let lane = queue.join("worker", rrffmpeg::Class::Heavy, 0, "client", 100);
    let lane_dir = only_subdir(&coord).join("queue");

    // Fill the lane with tickets in the queue's own on-disk format:
    // `class weight seq client`.
    for i in 0..TICKETS {
        let seq = 1_000_000 + i;
        let path = lane_dir.join(format!("{seq}-{}.want", std::process::id()));
        std::fs::write(path, format!("0 100 {seq} client\n")).expect("write ticket");
    }
    (base, queue, lane)
}

/// Rank and scan every ticket in the lane.
fn scan(c: &mut Criterion) {
    let (base, queue, lane) = busy_lane("queue-scan");
    c.bench_function("queue_rank_scan_10000", |b| {
        b.iter(|| std::hint::black_box(queue.before_me(&lane)));
    });
    drop(lane);
    let _ = std::fs::remove_dir_all(&base);
}

/// Publish a ticket and remove it: the wait/unwait cost.
fn join(c: &mut Criterion) {
    let (base, queue, lane) = busy_lane("queue-join");
    c.bench_function("queue_join_drop", |b| {
        b.iter(|| {
            let ticket = queue.join("worker", rrffmpeg::Class::Heavy, 0, "client", 100);
            drop(ticket);
        });
    });
    drop(lane);
    let _ = std::fs::remove_dir_all(&base);
}

/// The single host key directory under `coord`.
fn only_subdir(coord: &Path) -> PathBuf {
    std::fs::read_dir(coord)
        .expect("read coord dir")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("one host dir under coord")
}

criterion_group! {
    name = scan_benches;
    config = Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(10));
    targets = scan
}
criterion_group!(join_benches, join);
criterion_main!(scan_benches, join_benches);
