//! Placement: which worker runs this job, and is there a free slot?
//!
//! Slots live in the state dir as flock files, so the cap holds across the
//! one-process-per-job model Jellyfin uses.

use crate::Host;
use crate::Job;
use crate::locks::{LockDir, SlotGuard};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Waits used while polling for a slot. A negative priority polls fast, the
/// rest back off so co-scheduled jobs do not hammer the state dir.
const FAST_POLL: Duration = Duration::from_millis(20);
const SLOW_POLL_START: Duration = Duration::from_millis(40);
const SLOW_POLL_MAX: Duration = Duration::from_millis(400);

/// Passes over the host list before `acquire` gives up, so a lost race cannot
/// spin forever.
const ACQUIRE_PASSES: usize = 2;

#[derive(Debug)]
pub struct Lease {
    pub host: Host,
    /// Frees the slot when the lease drops.
    _guard: SlotGuard,
}

pub struct Scheduler {
    hosts: Vec<Host>,
    locks: Arc<LockDir>,
    /// How long this process's down verdict is trusted by the others.
    down_ttl: Duration,
}

impl Scheduler {
    pub fn new(hosts: Vec<Host>, locks: Arc<LockDir>, down_ttl: Duration) -> Scheduler {
        Scheduler {
            hosts,
            locks,
            down_ttl,
        }
    }

    /// Reserve a slot on the eligible host with the lowest running/concurrency
    /// ratio, first in config order on ties.
    pub fn acquire(&self, job: &Job) -> Option<Lease> {
        for _ in 0..ACQUIRE_PASSES {
            let index = self.pick_host(job.capability.as_deref())?;
            let host = &self.hosts[index];
            if let Some(guard) = self.locks.try_acquire(host, &job.id) {
                return Some(Lease {
                    host: host.clone(),
                    _guard: guard,
                });
            }
            // Another process won that slot: recompute on the next pass.
        }
        None
    }

    /// Poll for a slot until the deadline. Priority only sets the poll rate,
    /// because ordering across processes would need a daemon.
    pub fn acquire_blocking(&self, job: &Job, timeout: Duration) -> Option<Lease> {
        let deadline = Instant::now() + timeout;
        let mut wait = base_interval(job.priority);
        loop {
            if let Some(lease) = self.acquire(job) {
                return Some(lease);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            thread::sleep(wait.min(deadline - now));
            wait = next_interval(job.priority, wait);
        }
    }

    pub fn mark_down(&self, host: &str) {
        self.locks.mark_down(host, self.down_ttl);
    }

    pub fn mark_up(&self, host: &str) {
        self.locks.mark_up(host);
    }

    /// Every configured worker, in config order.
    pub fn hosts(&self) -> Vec<Host> {
        self.hosts.clone()
    }

    pub fn is_down(&self, host: &str) -> bool {
        self.locks.is_down(host)
    }

    fn capability_ok(&self, host: &Host, capability: Option<&str>) -> bool {
        match capability {
            None => true,
            Some(need) => {
                host.capabilities.is_empty() || host.capabilities.iter().any(|c| c == need)
            }
        }
    }

    /// Best eligible host with a free slot, or None. A down host is never
    /// eligible.
    fn pick_host(&self, capability: Option<&str>) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (index, host) in self.hosts.iter().enumerate() {
            if self.locks.is_down(&host.name) {
                continue;
            }
            if !self.capability_ok(host, capability) {
                continue;
            }
            let running = self.locks.occupancy(host);
            if running >= host.concurrency {
                continue;
            }
            best = match best {
                None => Some((index, running)),
                Some((current, current_running)) => {
                    let cur = &self.hosts[current];
                    let candidate = running as u128 * cur.concurrency as u128;
                    let incumbent = current_running as u128 * host.concurrency as u128;
                    if candidate < incumbent {
                        Some((index, running))
                    } else {
                        Some((current, current_running))
                    }
                }
            };
        }
        best.map(|(index, _)| index)
    }
}

/// First wait before the next slot retry.
fn base_interval(priority: i32) -> Duration {
    if priority < 0 {
        FAST_POLL
    } else {
        SLOW_POLL_START
    }
}

/// Next wait: a probe stays at 20 ms, a transcode doubles up to 400 ms. The
/// 0..10 ms of jitter keeps peers from retrying in lockstep.
fn next_interval(priority: i32, previous: Duration) -> Duration {
    if priority < 0 {
        return FAST_POLL;
    }
    let doubled = (previous * 2).min(SLOW_POLL_MAX);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    doubled + Duration::from_millis(u64::from(nanos % 11))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Program;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    fn host(name: &str, concurrency: usize, capabilities: &[&str]) -> Host {
        Host {
            name: name.to_string(),
            concurrency,
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            ffmpeg_bin: None,
            ffprobe_bin: None,
            agent_addr: None,
            paths: Vec::new(),
        }
    }

    fn job(priority: i32, capability: Option<&str>) -> Job {
        Job {
            id: "job".to_string(),
            program: Program::Ffmpeg,
            args: Vec::new(),
            priority,
            capability: capability.map(|c| c.to_string()),
        }
    }

    fn scratch(tag: &str) -> (std::path::PathBuf, Arc<LockDir>) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rrffmpeg-scheduler-{}-{n}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let locks = Arc::new(LockDir::new(&dir));
        (dir, locks)
    }

    fn build(hosts: Vec<Host>, tag: &str) -> (std::path::PathBuf, Scheduler) {
        let (dir, locks) = scratch(tag);
        (dir, Scheduler::new(hosts, locks, Duration::from_secs(60)))
    }

    #[test]
    fn concurrency_cap_is_never_exceeded() {
        let (dir, locks) = scratch("cap");
        let scheduler = Arc::new(Scheduler::new(
            vec![host("w", 2, &[])],
            locks,
            Duration::from_secs(60),
        ));
        let held = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let scheduler = Arc::clone(&scheduler);
            let held = Arc::clone(&held);
            let peak = Arc::clone(&peak);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let lease = scheduler
                    .acquire_blocking(&job(0, None), Duration::from_secs(5))
                    .expect("a slot within five seconds");
                let now = held.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                held.fetch_sub(1, Ordering::SeqCst);
                drop(lease);
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread");
        }
        assert!(peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(held.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capability_match_skips_the_wrong_host() {
        let (dir, scheduler) = build(
            vec![host("vaapi", 1, &["vaapi"]), host("nvenc", 1, &["nvenc"])],
            "capability",
        );
        let lease = scheduler.acquire(&job(0, Some("nvenc"))).expect("a lease");
        assert_eq!(lease.host.name, "nvenc");
        drop(lease);
        let _ = std::fs::remove_dir_all(dir);

        // Empty capabilities on a host accept any job.
        let (dir, open) = build(vec![host("open", 1, &[])], "open");
        let lease = open.acquire(&job(0, Some("qsv"))).expect("a lease");
        assert_eq!(lease.host.name, "open");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn down_host_is_skipped_and_mark_up_restores_it() {
        let (dir, scheduler) = build(vec![host("w", 1, &[])], "down");
        scheduler.mark_down("w");
        assert!(scheduler.is_down("w"));
        assert!(scheduler.acquire(&job(0, None)).is_none());

        scheduler.mark_up("w");
        assert!(!scheduler.is_down("w"));
        let lease = scheduler.acquire(&job(0, None)).expect("a lease");
        assert_eq!(lease.host.name, "w");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_spreads_across_hosts_before_filling_one() {
        let (dir, scheduler) = build(vec![host("a", 2, &[]), host("b", 2, &[])], "spread");
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(scheduler.acquire(&job(0, None)).expect("a lease"));
        }
        let order: Vec<&str> = leases.iter().map(|l| l.host.name.as_str()).collect();
        assert_eq!(order, ["a", "b", "a", "b"]);
        assert!(scheduler.acquire(&job(0, None)).is_none());
        drop(leases);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn blocking_acquire_times_out_then_wakes_on_release() {
        let (dir, locks) = scratch("blocking");
        let scheduler = Arc::new(Scheduler::new(
            vec![host("w", 1, &[])],
            locks,
            Duration::from_secs(60),
        ));
        let held = scheduler.acquire(&job(0, None)).expect("the only slot");
        assert!(
            scheduler
                .acquire_blocking(&job(0, None), Duration::from_millis(50))
                .is_none()
        );

        let waiter = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, None), Duration::from_secs(2)))
        };
        thread::sleep(Duration::from_millis(50));
        drop(held);
        assert!(waiter.join().expect("waiter thread").is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn poll_intervals_are_bounded_and_respect_priority() {
        // A probe polls faster than a transcode: lower priority waits less.
        assert!(base_interval(-5) < base_interval(0));
        assert!(base_interval(0) <= base_interval(5));

        let mut wait = base_interval(0);
        for _ in 0..10 {
            let next = next_interval(0, wait);
            assert!(
                next >= SLOW_POLL_START,
                "the backoff shrank below its floor"
            );
            assert!(
                next <= SLOW_POLL_MAX + Duration::from_millis(10),
                "the backoff grew past its ceiling"
            );
            wait = next;
        }
        assert_eq!(next_interval(-5, Duration::from_secs(1)), FAST_POLL);
    }
}
