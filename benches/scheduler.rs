//! Scheduler cost: take and free a slot, alone and under contention.

#[path = "harness/mod.rs"]
mod harness;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use rrffmpeg::locks::LockDir;
use rrffmpeg::scheduler::Scheduler;
use rrffmpeg::{Job, Program};
use std::sync::{Arc, Barrier};
use std::time::Duration;

/// Contending threads in the parallel bench.
const THREADS: usize = 8;

/// Acquire/release pairs each thread runs.
const PAIRS: usize = 100;

fn job() -> Job {
    Job {
        id: "job".to_string(),
        program: Program::Ffmpeg,
        args: Vec::new(),
        priority: 0,
        capabilities: Vec::new(),
    }
}

fn bench(c: &mut Criterion) {
    let base = harness::temp_dir("scheduler");
    let locks = Arc::new(LockDir::new(&base));
    // Sixteen slots, so eight threads never block on capacity: this measures
    // the slot file and lock cost, not the queue.
    let scheduler = Scheduler::new(
        vec![harness::host("worker", 16)],
        locks,
        Duration::from_secs(60),
    );

    let solo = job();
    c.bench_function("scheduler_acquire_release", |b| {
        b.iter(|| {
            let lease = scheduler.acquire(&solo).expect("a free slot");
            drop(lease);
        });
    });

    // One iteration is 8 threads times 100 pairs. The threads start blocked on
    // a barrier in the untimed setup, so thread creation stays out of the
    // measurement; the routine releases them and joins.
    let scheduler = Arc::new(scheduler);
    let mut group = c.benchmark_group("scheduler_acquire_release");
    group.throughput(Throughput::Elements((THREADS * PAIRS) as u64));
    group.bench_function("8_threads", |b| {
        let scheduler = Arc::clone(&scheduler);
        b.iter_batched(
            move || {
                let barrier = Arc::new(Barrier::new(THREADS + 1));
                let mut handles = Vec::with_capacity(THREADS);
                for _ in 0..THREADS {
                    let scheduler = Arc::clone(&scheduler);
                    let barrier = Arc::clone(&barrier);
                    handles.push(std::thread::spawn(move || {
                        let job = job();
                        barrier.wait();
                        for _ in 0..PAIRS {
                            let lease = scheduler.acquire(&job).expect("a free slot");
                            drop(lease);
                        }
                    }));
                }
                (barrier, handles)
            },
            |(barrier, handles)| {
                barrier.wait();
                for handle in handles {
                    handle.join().expect("bench thread");
                }
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&base);
}

criterion_group!(benches, bench);
criterion_main!(benches);
