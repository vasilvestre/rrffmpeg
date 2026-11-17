//! The weight must change who starts, not only who ranks first. Two clients
//! bring more jobs than one slot can serve, and the start order must follow
//! the weight ratio while both still have work.

use rrffmpeg::config::Host;
use rrffmpeg::locks::LockDir;
use rrffmpeg::scheduler::Scheduler;
use rrffmpeg::{Job, Program};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const JOBS_PER_CLIENT: usize = 20;
/// How many starts to judge. Both clients still have jobs left in this window,
/// so the split reflects the queue and not who ran out of work first.
const WINDOW: usize = 20;

fn worker() -> Host {
    Host {
        name: "worker".into(),
        concurrency: 1,
        capabilities: Vec::new(),
        ffmpeg_bin: None,
        ffprobe_bin: None,
        agent_addr: None,
        paths: Vec::new(),
    }
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
    let heavy = build("heavy", 400);
    let light = build("light", 100);
    let order = Arc::new(Mutex::new(Vec::new()));

    let mut handles = Vec::new();
    for i in 0..2 * JOBS_PER_CLIENT {
        let (scheduler, name) = if i % 2 == 0 {
            (heavy.clone(), "heavy")
        } else {
            (light.clone(), "light")
        };
        let order = order.clone();
        handles.push(std::thread::spawn(move || {
            let job = Job {
                id: format!("{name}-{i}"),
                program: Program::Ffmpeg,
                args: Vec::new(),
                priority: 0,
                capability: None,
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
    assert!(order.len() == 2 * JOBS_PER_CLIENT, "every job starts once");
    let window = &order[..WINDOW];
    let heavy = window.iter().filter(|name| **name == "heavy").count();
    let light = window.len() - heavy;
    assert!(
        heavy >= 3 * light,
        "first {WINDOW} starts: heavy {heavy}, light {light}, expected about 4 to 1"
    );
}
