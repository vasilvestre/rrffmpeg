//! Placement: which worker runs this job, and is there a free slot?
//!
//! Slots live in the state dir as flock files, so the cap holds across the
//! one-process-per-job model Jellyfin uses.

use crate::Host;
use crate::Job;
use crate::locks::{LockDir, SlotGuard};
use crate::queue::{Lane, Queue};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Waits used while polling for a slot. A negative priority polls fast, the
/// rest back off so co-scheduled jobs do not hammer the state dir.
///
/// The fast wait is the latency a queued job pays when a slot frees, so it is
/// short: one pass costs about 50 us, and only the job at the head of a lane
/// pays it. The first slow wait is likewise short, because a job two places
/// back must not sleep through its own turn.
const FAST_POLL: Duration = Duration::from_millis(2);
const SLOW_POLL_START: Duration = Duration::from_millis(10);
const SLOW_POLL_MAX: Duration = Duration::from_millis(400);

/// A ticket older than 3 s is dropped as dead, so refreshing it on every fast
/// poll is 500 times more often than needed.
const REFRESH_EVERY: Duration = Duration::from_millis(500);

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
    /// The shared order, under the same `coord/` as the slots.
    queue: Queue,
    /// Name this process reports to the queue. Two clients that share a
    /// `state_dir` must not share a name.
    client: String,
    /// Share of a busy worker, 1 to 1000. Only read while jobs wait.
    weight: u32,
    /// How long this process's down verdict is trusted by the others.
    down_ttl: Duration,
}

impl Scheduler {
    pub fn new(hosts: Vec<Host>, locks: Arc<LockDir>, down_ttl: Duration) -> Scheduler {
        let queue = Queue::new(locks.coord());
        Scheduler {
            hosts,
            locks,
            queue,
            client: "client".to_string(),
            weight: 100,
            down_ttl,
        }
    }

    /// Name and share of this client. The weight is clamped to the range the
    /// kernel's own IO weights use, so one config cannot drown another.
    pub fn with_client(mut self, client: impl Into<String>, weight: u32) -> Scheduler {
        self.client = client.into();
        self.weight = weight.clamp(1, 1000);
        self
    }

    /// Reserve a slot on the eligible host with the lowest running/concurrency
    /// ratio, first in config order on ties.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub fn acquire(&self, job: &Job) -> Option<Lease> {
        // Someone already waits for a slot this job could use, so queue up
        // behind them. A caller that skips this takes the slot a waiter slept
        // through, and a stream of fresh callers can starve it for good.
        for host in &self.hosts {
            if !self.locks.is_down(&host.name)
                && self.capability_ok(host, job.capability.as_deref())
                && self.queue.has_waiters(&host.name)
            {
                return None;
            }
        }
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

    /// Poll for a slot until the deadline. The first attempt is unqueued, so a
    /// job that finds a free slot never touches the queue directory. After
    /// that the job publishes one ticket per eligible host and declines any
    /// slot a better ticket is waiting for: class first, then the client's
    /// share of recent starts.
    pub fn acquire_blocking(&self, job: &Job, timeout: Duration) -> Option<Lease> {
        let deadline = Instant::now() + timeout;
        if let Some(lease) = self.acquire(job) {
            return Some(lease);
        }
        let lanes = self.lanes(job);
        let mut wait = base_interval(job.priority);
        let mut refresh_due = Instant::now() + REFRESH_EVERY;
        loop {
            let mut ready = false;
            let mut free = false;
            // Refresh every ticket, not only the one for a host with a free
            // slot: a full host can stay full for the length of a transcode,
            // and a ticket nobody refreshed is dropped as dead after 3 s.
            if Instant::now() >= refresh_due {
                for lane in lanes.values() {
                    self.queue.refresh(lane);
                }
                refresh_due = Instant::now() + REFRESH_EVERY;
            }
            if let Some(index) = self.pick_host(job.capability.as_deref()) {
                // A free slot means the queue is draining now, so poll fast
                // even if a ticket still ranks ahead: otherwise a job sleeps
                // through its own turn at the backoff rate, and a loaded
                // worker hands out one slot per backoff step.
                free = true;
                let host = &self.hosts[index];
                if let Some(lane) = lanes.get(&host.name) {
                    // Pass when fewer tickets rank ahead than there are slots
                    // free right now: the first jobs go at once, in order.
                    let open = host.concurrency.saturating_sub(self.locks.occupancy(host));
                    ready = self.queue.before_me(lane) < open;
                    if ready && let Some(guard) = self.locks.try_acquire(host, &job.id) {
                        // Only a job that had to wait is charged, so the
                        // shares measure contention and not idle history.
                        self.queue.charge(&host.name, &self.client);
                        return Some(Lease {
                            host: host.clone(),
                            _guard: guard,
                        });
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let pause = if ready || free { FAST_POLL } else { wait };
            thread::sleep(pause.min(deadline - now));
            wait = next_interval(job.priority, wait);
        }
    }

    /// One ticket per host this job could use. A ticket for a full host still
    /// matters: it is how the other waiters see this job coming.
    fn lanes(&self, job: &Job) -> HashMap<String, Lane> {
        let mut lanes = HashMap::new();
        for host in &self.hosts {
            if self.is_down(&host.name) || !self.capability_ok(host, job.capability.as_deref()) {
                continue;
            }
            let lane = self
                .queue
                .join(&host.name, job.priority, &self.client, self.weight);
            lanes.insert(host.name.clone(), lane);
        }
        lanes
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
            ssh_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
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
        let _serial = crate::exclusive();
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
        let _serial = crate::exclusive();
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
        let _serial = crate::exclusive();
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
        let _serial = crate::exclusive();
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
    fn a_probe_takes_the_slot_a_transcode_is_waiting_for() {
        let (dir, locks) = scratch("probe-first");
        let _serial = crate::exclusive();
        let scheduler = Arc::new(Scheduler::new(
            vec![host("w", 1, &[])],
            locks,
            Duration::from_secs(60),
        ));
        let held = scheduler.acquire(&job(0, None)).expect("the only slot");

        let transcode = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, None), Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(200));
        let probe = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || {
                scheduler.acquire_blocking(&job(-5, None), Duration::from_secs(5))
            })
        };
        thread::sleep(Duration::from_millis(200));
        drop(held);

        // The probe joined last and still goes first: class decides, not the
        // poll rate.
        let probe_lease = probe.join().expect("probe thread").expect("the probe wins");
        assert_eq!(probe_lease.host.name, "w");
        drop(probe_lease);
        let transcode_lease = transcode
            .join()
            .expect("transcode thread")
            .expect("the transcode gets the slot next");
        drop(transcode_lease);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_newcomer_does_not_take_the_slot_a_waiter_queued_for() {
        let (dir, locks) = scratch("no-jump");
        let _serial = crate::exclusive();
        let scheduler = Arc::new(Scheduler::new(
            vec![host("w", 2, &[])],
            locks,
            Duration::from_secs(60),
        ));
        let first = scheduler.acquire(&job(0, None)).expect("first slot");
        let second = scheduler.acquire(&job(0, None)).expect("second slot");

        // Both slots are held, so this one has to publish a ticket and wait.
        let waiter = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, None), Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(100));
        assert!(
            scheduler.queue.has_waiters("w"),
            "the waiter must publish a ticket"
        );

        // A slot frees while the waiter sleeps out its poll interval. Without
        // the check the newcomer takes it and the waiter loses its turn.
        drop(second);
        assert!(
            scheduler.acquire(&job(0, None)).is_none(),
            "a newcomer must queue behind the waiter"
        );

        drop(first);
        let lease = waiter.join().expect("waiter thread");
        assert!(lease.is_some(), "the waiter gets the free slot");
        drop(lease);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_job_that_had_to_wait_is_charged_to_its_client() {
        let (dir, locks) = scratch("charge");
        let _serial = crate::exclusive();
        let scheduler = Arc::new(
            Scheduler::new(
                vec![host("w", 1, &[])],
                Arc::clone(&locks),
                Duration::from_secs(60),
            )
            .with_client("alice", 300),
        );
        let held = scheduler.acquire(&job(0, None)).expect("the only slot");
        let waiter = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, None), Duration::from_secs(2)))
        };
        thread::sleep(Duration::from_millis(150));
        drop(held);
        drop(waiter.join().expect("waiter thread").expect("a lease"));

        let served = locks
            .coord()
            .join(crate::locks::hostkey("w"))
            .join("queue/served");
        let text = std::fs::read_to_string(&served).expect("a start is charged");
        assert_eq!(text.trim(), "1 alice");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_weight_is_clamped_to_the_kernel_range() {
        let (dir, locks) = scratch("weight-clamp");
        let _serial = crate::exclusive();
        let low = Scheduler::new(vec![host("w", 1, &[])], locks, Duration::from_secs(60))
            .with_client("alice", 0);
        assert_eq!(low.weight, 1);
        assert_eq!(low.with_client("alice", 5000).weight, 1000);
        assert_eq!(
            Scheduler::new(
                vec![],
                Arc::new(LockDir::new(&dir)),
                Duration::from_secs(60)
            )
            .weight,
            100
        );
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
