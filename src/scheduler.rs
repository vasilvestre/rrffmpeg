//! Placement: which worker runs this job, and is there a free slot?
//!
//! Slots live in the state dir as flock files, so the cap holds across the
//! one-process-per-job model Jellyfin uses.

use crate::Host;
use crate::Job;
use crate::locks::{LockDir, SlotGuard};
use crate::paths;
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

    /// Reserve a slot on the eligible host with the best placement order, see
    /// [`beats`].
    pub fn acquire(&self, job: &Job) -> Option<Lease> {
        // Someone already waits for a slot this job could use, so queue up
        // behind them. A caller that skips this takes the slot a waiter slept
        // through, and a stream of fresh callers can starve it for good.
        for host in &self.hosts {
            if self.eligible(host, job) && self.queue.has_waiters(&host.name) {
                return None;
            }
        }
        for _ in 0..ACQUIRE_PASSES {
            let index = self.pick_host(job)?;
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
            if let Some(index) = self.pick_host(job) {
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
            if !self.eligible(host, job) {
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

    /// Jobs running on `host` right now, this one included. Reads the slot
    /// locks rather than the journal, so recording a start never replays the
    /// journal on the dispatch path.
    pub fn running(&self, host: &Host) -> usize {
        self.locks.occupancy(host)
    }

    /// Every capability the job needs must be advertised. One match is not
    /// enough: a host that encodes H.264 in hardware can still lack 10-bit
    /// HEVC or tone mapping.
    fn capability_ok(&self, host: &Host, need: &[String]) -> bool {
        need.is_empty()
            || host.capabilities.is_empty()
            || need.iter().all(|n| host.capabilities.contains(n))
    }

    /// A host that is up, advertises every token the job needs and declares a
    /// root for every input. Slots are checked separately: a full host still
    /// earns a ticket.
    fn eligible(&self, host: &Host, job: &Job) -> bool {
        !self.locks.is_down(&host.name)
            && self.capability_ok(host, job.capabilities.as_slice())
            && paths::reads_inputs(host, &job.args)
    }

    /// Best eligible host with a free slot, or None.
    fn pick_host(&self, job: &Job) -> Option<usize> {
        let mut best: Option<(usize, usize)> = None;
        for (index, host) in self.hosts.iter().enumerate() {
            if !self.eligible(host, job) {
                continue;
            }
            let running = self.locks.occupancy(host);
            if running >= host.concurrency {
                continue;
            }
            best = match best {
                None => Some((index, running)),
                Some((current, current_running)) => {
                    if beats(host, running, &self.hosts[current], current_running, job) {
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

/// Whether `host` beats `incumbent` for this job: a host that needs no ssh
/// when the job needs no accelerator, then a higher `prefer`, then the lower
/// running/concurrency ratio, then the name. Declaration order is never a
/// rule, so swapping two `[[hosts]]` blocks changes nothing (ADR 0031).
fn beats(
    host: &Host,
    running: usize,
    incumbent: &Host,
    incumbent_running: usize,
    job: &Job,
) -> bool {
    if job.capabilities.is_empty() {
        let direct = needs_no_ssh(host);
        if direct != needs_no_ssh(incumbent) {
            return direct;
        }
    }
    if host.prefer != incumbent.prefer {
        return host.prefer > incumbent.prefer;
    }
    let candidate = running as u128 * incumbent.concurrency as u128;
    let current = incumbent_running as u128 * host.concurrency as u128;
    if candidate != current {
        return candidate < current;
    }
    host.name < incumbent.name
}

/// No handshake, no remote shell: the client itself, or an agent socket.
fn needs_no_ssh(host: &Host) -> bool {
    host.local || host.agent_addr.is_some()
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
            local: false,
            name: name.to_string(),
            concurrency,
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
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

    fn job(priority: i32, capability: &[&str]) -> Job {
        Job {
            id: "job".to_string(),
            program: Program::Ffmpeg,
            args: Vec::new(),
            priority,
            capabilities: capability.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// A job with a real input, so `reads` has something to judge.
    fn input_job(input: &str, capability: &[&str]) -> Job {
        Job {
            args: vec![
                std::ffi::OsString::from("-i"),
                std::ffi::OsString::from(input),
            ],
            ..job(0, capability)
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
                    .acquire_blocking(&job(0, &[]), Duration::from_secs(5))
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
        let lease = scheduler.acquire(&job(0, &["nvenc"])).expect("a lease");
        assert_eq!(lease.host.name, "nvenc");
        drop(lease);
        let _ = std::fs::remove_dir_all(dir);

        // Empty capabilities on a host accept any job.
        let (dir, open) = build(vec![host("open", 1, &[])], "open");
        let lease = open.acquire(&job(0, &["qsv"])).expect("a lease");
        assert_eq!(lease.host.name, "open");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The production failure: the host that held no root for the input took
    /// the job, ffmpeg exited 254, and nothing retried it.
    #[test]
    fn an_input_the_first_host_cannot_read_lands_on_the_second() {
        let (dir, locks) = scratch("reads");
        let _serial = crate::exclusive();
        let scheduler = Scheduler::new(
            vec![
                // Declared first, and it reads only the client's media root.
                readable(host("gpu", 1, &[]), &["/home/bob/media"]),
                // Declared second, and it holds the archive under another root.
                readable(archive(), &["/mnt/archive"]),
            ],
            locks,
            Duration::from_secs(60),
        );
        let lease = scheduler
            .acquire(&input_job("file:/archive/film.mkv", &[]))
            .expect("the second host can read it");
        assert_eq!(lease.host.name, "archive");
        drop(lease);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two orders, one answer: file order is not a routing rule.
    #[test]
    fn swapping_two_host_blocks_does_not_change_the_host() {
        let _serial = crate::exclusive();
        for flip in [false, true] {
            let (dir, locks) = scratch("swap");
            let gpu = readable(host("gpu", 1, &[]), &["/home/bob/media"]);
            let archive = readable(archive(), &["/mnt/archive"]);
            let hosts = if flip {
                vec![archive, gpu]
            } else {
                vec![gpu, archive]
            };
            let scheduler = Scheduler::new(hosts, locks, Duration::from_secs(60));
            let lease = scheduler
                .acquire(&input_job("file:/archive/film.mkv", &[]))
                .expect("a lease");
            assert_eq!(lease.host.name, "archive", "flip {flip}");
            drop(lease);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The declared preference wins over the name, and the name breaks a full
    /// tie, so no order of the blocks can change the answer.
    #[test]
    fn prefer_orders_equal_hosts_and_the_name_breaks_a_full_tie() {
        let _serial = crate::exclusive();
        for flip in [false, true] {
            let (dir, locks) = scratch("prefer");
            let mut alpha = host("alpha", 1, &[]);
            alpha.prefer = 0;
            let mut zulu = host("zulu", 1, &[]);
            zulu.prefer = 10;
            let hosts = if flip {
                vec![zulu.clone(), alpha.clone()]
            } else {
                vec![alpha.clone(), zulu.clone()]
            };
            let scheduler = Scheduler::new(hosts, locks, Duration::from_secs(60));
            let lease = scheduler.acquire(&job(0, &[])).expect("a lease");
            assert_eq!(lease.host.name, "zulu", "higher prefer wins, flip {flip}");
            drop(lease);
            let _ = std::fs::remove_dir_all(dir);
        }
        let (dir, locks) = scratch("name-tie");
        let scheduler = Scheduler::new(
            vec![host("zulu", 1, &[]), host("alpha", 1, &[])],
            locks,
            Duration::from_secs(60),
        );
        let lease = scheduler.acquire(&job(0, &[])).expect("a lease");
        assert_eq!(lease.host.name, "alpha");
        drop(lease);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A software encode needs nothing from the network, so it stays where it
    /// is even when the worker holds the only GPU.
    #[test]
    fn a_software_job_with_a_local_host_stays_local() {
        let (dir, locks) = scratch("local-x264");
        let _serial = crate::exclusive();
        let mut here = host("client", 1, &["vaapi"]);
        here.local = true;
        let scheduler = Scheduler::new(
            vec![host("n100", 2, &["vaapi", "nvenc"]), here],
            locks,
            Duration::from_secs(60),
        );
        let lease = scheduler
            .acquire(&input_job("/home/alice/film.mkv", &[]))
            .expect("a lease");
        assert_eq!(lease.host.name, "client", "libx264 stays on the client");
        drop(lease);
        let lease = scheduler.acquire(&job(0, &["nvenc"])).expect("a lease");
        assert_eq!(lease.host.name, "n100", "an nvenc job still goes out");
        drop(lease);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn readable(mut host: Host, roots: &[&str]) -> Host {
        host.reads = roots.iter().map(std::path::PathBuf::from).collect();
        host
    }

    /// A host that mounts the client's `/archive` under `/mnt/archive`.
    fn archive() -> Host {
        let mut host = host("archive", 1, &[]);
        host.paths = vec![crate::config::PathMap {
            from: "/archive".into(),
            to: "/mnt/archive".into(),
        }];
        host
    }

    #[test]
    fn running_counts_the_held_lease() {
        let (dir, scheduler) = build(vec![host("w", 2, &[])], "running");
        let _serial = crate::exclusive();
        let lease = scheduler.acquire(&job(0, &[])).expect("a lease");
        let host = lease.host.clone();
        // The lease already holds one slot, so the count includes this job.
        assert_eq!(scheduler.running(&host), 1);
        let second = scheduler.acquire(&job(0, &[])).expect("a second slot");
        assert_eq!(scheduler.running(&host), 2);
        drop(second);
        drop(lease);
        assert_eq!(scheduler.running(&host), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn down_host_is_skipped_and_mark_up_restores_it() {
        let (dir, scheduler) = build(vec![host("w", 1, &[])], "down");
        let _serial = crate::exclusive();
        scheduler.mark_down("w");
        assert!(scheduler.is_down("w"));
        assert!(scheduler.acquire(&job(0, &[])).is_none());

        scheduler.mark_up("w");
        assert!(!scheduler.is_down("w"));
        let lease = scheduler.acquire(&job(0, &[])).expect("a lease");
        assert_eq!(lease.host.name, "w");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_spreads_across_hosts_before_filling_one() {
        let (dir, scheduler) = build(vec![host("a", 2, &[]), host("b", 2, &[])], "spread");
        let _serial = crate::exclusive();
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(scheduler.acquire(&job(0, &[])).expect("a lease"));
        }
        let order: Vec<&str> = leases.iter().map(|l| l.host.name.as_str()).collect();
        assert_eq!(order, ["a", "b", "a", "b"]);
        assert!(scheduler.acquire(&job(0, &[])).is_none());
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
        let held = scheduler.acquire(&job(0, &[])).expect("the only slot");
        assert!(
            scheduler
                .acquire_blocking(&job(0, &[]), Duration::from_millis(50))
                .is_none()
        );

        let waiter = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, &[]), Duration::from_secs(2)))
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
        let held = scheduler.acquire(&job(0, &[])).expect("the only slot");

        let transcode = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, &[]), Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(200));
        let probe = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(-5, &[]), Duration::from_secs(5)))
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
        let first = scheduler.acquire(&job(0, &[])).expect("first slot");
        let second = scheduler.acquire(&job(0, &[])).expect("second slot");

        // Both slots are held, so this one has to publish a ticket and wait.
        let waiter = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, &[]), Duration::from_secs(5)))
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
            scheduler.acquire(&job(0, &[])).is_none(),
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
        let held = scheduler.acquire(&job(0, &[])).expect("the only slot");
        let waiter = {
            let scheduler = Arc::clone(&scheduler);
            thread::spawn(move || scheduler.acquire_blocking(&job(0, &[]), Duration::from_secs(2)))
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

    /// A zero timeout must not turn into a poll: a free slot is taken at once
    /// and a full host returns `None` on the first pass.
    #[test]
    fn acquire_blocking_with_zero_wait_returns_immediately() {
        let (dir, scheduler) = build(vec![host("w", 1, &[])], "zero-wait");
        let _serial = crate::exclusive();

        let start = Instant::now();
        let lease = scheduler
            .acquire_blocking(&job(0, &[]), Duration::ZERO)
            .expect("a free slot is taken without waiting");
        assert!(start.elapsed() < Duration::from_secs(1));

        let start = Instant::now();
        assert!(
            scheduler
                .acquire_blocking(&job(0, &[]), Duration::ZERO)
                .is_none(),
            "a full host returns at once, it does not poll"
        );
        assert!(start.elapsed() < Duration::from_secs(1));

        drop(lease);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An empty capability list is the host saying it can run anything, so a
    /// job that needs several capabilities still lands there.
    #[test]
    fn a_host_with_no_capabilities_takes_every_job() {
        let (dir, scheduler) = build(vec![host("open", 1, &[])], "any-capability");
        let _serial = crate::exclusive();

        let lease = scheduler
            .acquire(&job(0, &["vaapi", "hevc", "tonemap"]))
            .expect("a host with no declared capabilities accepts any job");
        assert_eq!(lease.host.name, "open");

        drop(lease);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The janitor probes a host every cycle. A failed probe while the verdict
    /// is already down must not push the expiry out, or the host stays down
    /// forever.
    #[test]
    fn down_verdict_expiry_is_not_reset_by_a_probe_failure() {
        let (dir, locks) = scratch("down-expiry");
        let _serial = crate::exclusive();
        let scheduler = Scheduler::new(
            vec![host("w", 1, &[])],
            Arc::clone(&locks),
            Duration::from_secs(5),
        );
        let down = locks.coord().join(crate::locks::hostkey("w")).join("down");

        scheduler.mark_down("w");
        let first = std::fs::read_to_string(&down).expect("the verdict is written");

        // Failed probes: the host is down, so every acquire returns nothing.
        for _ in 0..5 {
            assert!(scheduler.acquire(&job(0, &[])).is_none());
            assert!(scheduler.is_down("w"));
            // Long enough that a rewritten expiry would land on a new ms.
            thread::sleep(Duration::from_millis(5));
        }

        let after = std::fs::read_to_string(&down).expect("the verdict is still there");
        assert_eq!(
            first, after,
            "a failed probe must not extend the down verdict"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_acquires_do_not_report_a_full_host_while_slots_are_free() {
        // Eight threads share sixteen slots, so at least eight are always free.
        // Occupancy probes used to take the same exclusive lock as a holder,
        // and eight probes plus eight holders made the host read as full.
        let (dir, scheduler) = build(vec![host("worker", 16, &[])], "parallel-free");
        let scheduler = Arc::new(scheduler);
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let scheduler = Arc::clone(&scheduler);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let job = job(0, &[]);
                barrier.wait();
                for _ in 0..1_000 {
                    let lease = scheduler.acquire(&job).expect("a free slot");
                    drop(lease);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("acquire thread");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
