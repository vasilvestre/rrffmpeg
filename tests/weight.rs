//! The weight must change who starts, not only who ranks first. Three clients
//! bring more jobs than one slot can serve, and the start order must follow
//! the weight ratio while all of them still have work.

use rrffmpeg::config::Host;
use rrffmpeg::locks::LockDir;
use rrffmpeg::scheduler::Scheduler;
use rrffmpeg::{Job, Program};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const JOBS_PER_CLIENT: usize = 20;
/// How many starts to judge. Every client still has jobs left in this window,
/// so the split reflects the queue and not who ran out of work first.
const WINDOW: usize = 21;

fn worker() -> Host {
    Host {
        name: "worker".into(),
        concurrency: 1,
        capabilities: Vec::new(),
        local: false,
        ffmpeg_bin: None,
        ffprobe_bin: None,
        agent_addr: None,
        ssh_args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        paths: Vec::new(),
        reads: Vec::new(),
        prefer: 0,
    }
}

/// Age of the single waiting ticket, or `None` when none exists.
fn ticket_age(dir: &std::path::Path) -> Option<Duration> {
    let host = std::fs::read_dir(dir.join("coord"))
        .ok()?
        .flatten()
        .next()?
        .path();
    let ticket = std::fs::read_dir(host.join("queue"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|e| e == "want"))?;
    let modified = std::fs::metadata(ticket).ok()?.modified().ok()?;
    std::time::SystemTime::now().duration_since(modified).ok()
}

/// A worker that is busy for the length of a transcode must not let the tickets
/// of the jobs waiting for it go stale: a stale ticket is dropped, and the
/// client that wakes first takes the slot regardless of its weight.
#[test]
fn a_ticket_stays_fresh_while_the_only_slot_is_busy() {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-fresh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let locks = Arc::new(LockDir::new(&dir));
    let scheduler = Arc::new(
        Scheduler::new(vec![worker()], locks, Duration::from_secs(60)).with_client("waiter", 100),
    );
    let job = |id: &str| Job {
        id: id.to_string(),
        program: Program::Ffmpeg,
        args: Vec::new(),
        priority: 0,
        capabilities: Vec::new(),
    };

    let holder = scheduler.clone();
    let held = std::thread::spawn(move || {
        let lease = holder
            .acquire_blocking(&job("holder"), Duration::from_secs(5))
            .expect("the holder takes the only slot");
        std::thread::sleep(Duration::from_millis(3300));
        drop(lease);
    });
    std::thread::sleep(Duration::from_millis(100));
    let waiter = scheduler.clone();
    let waited = std::thread::spawn(move || {
        let lease = waiter
            .acquire_blocking(&job("waiter"), Duration::from_secs(10))
            .expect("the waiter takes the slot once it frees");
        drop(lease);
    });

    std::thread::sleep(Duration::from_millis(3000));
    let age = ticket_age(&dir).expect("the waiter published a ticket");
    assert!(
        age < Duration::from_millis(1500),
        "the ticket is {age:?} old: the queue drops it as dead after 3 s"
    );

    held.join().expect("holder thread");
    waited.join().expect("waiter thread");
    let _ = std::fs::remove_dir_all(&dir);
}

fn charged_starts(dir: &std::path::Path) -> u64 {
    let host = std::fs::read_dir(dir.join("coord"))
        .expect("coord dir")
        .flatten()
        .next()
        .expect("one host dir")
        .path();
    let text = std::fs::read_to_string(host.join("queue").join("served")).expect("served file");
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(count, _)| count.parse::<u64>().ok())
        .sum()
}

#[test]
fn the_weight_sets_the_order_of_starts_under_contention() {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-weight-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let locks = Arc::new(LockDir::new(&dir));
    let build = |name: &str, weight: u32| {
        Arc::new(
            Scheduler::new(vec![worker()], locks.clone(), Duration::from_secs(60))
                .with_client(name, weight),
        )
    };
    // Shares of 4 : 2 : 1, so 21 starts should split about 12 : 6 : 3.
    let clients = [("heavy", 400u32), ("medium", 200u32), ("light", 100u32)];
    let schedulers: Vec<_> = clients
        .iter()
        .map(|(name, weight)| build(name, *weight))
        .collect();
    let order = Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::new();
    for i in 0..3 * JOBS_PER_CLIENT {
        let (name, _) = clients[i % clients.len()];
        let scheduler = schedulers[i % clients.len()].clone();
        let order = order.clone();
        handles.push(std::thread::spawn(move || {
            let job = Job {
                id: format!("{name}-{i}"),
                program: Program::Ffmpeg,
                args: Vec::new(),
                priority: 0,
                capabilities: Vec::new(),
            };
            let lease = scheduler
                .acquire_blocking(&job, Duration::from_secs(60))
                .expect("a slot within the timeout");
            order.lock().expect("order lock").push(name);
            // Long enough that the other jobs have to wait for it.
            std::thread::sleep(Duration::from_millis(40));
            drop(lease);
        }));
    }
    for handle in handles {
        handle.join().expect("worker thread");
    }

    let order = order.lock().expect("order lock").clone();
    let charged = charged_starts(&dir);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(charged >= 30, "only {charged} starts were charged");
    assert!(order.len() == 3 * JOBS_PER_CLIENT, "every job starts once");
    let window = &order[..WINDOW];
    let count = |name: &str| window.iter().filter(|seen| **seen == name).count();
    let (heavy, medium, light) = (count("heavy"), count("medium"), count("light"));

    // Expected share is window * weight / total weight.
    let total_weight: f64 = clients.iter().map(|(_, weight)| f64::from(*weight)).sum();
    for (name, weight, got) in [
        ("heavy", clients[0].1, heavy),
        ("medium", clients[1].1, medium),
        ("light", clients[2].1, light),
    ] {
        let expected = WINDOW as f64 * f64::from(weight) / total_weight;
        assert!(
            (got as f64 - expected).abs() <= 3.0,
            "first {WINDOW} starts: {name} {got}, expected about {expected:.1}"
        );
    }
    assert!(
        heavy >= 2 * light,
        "first {WINDOW} starts: heavy {heavy}, light {light}, expected a 4 to 1 split"
    );
}
