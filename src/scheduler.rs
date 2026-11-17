//! Placement: which worker runs this job, and is there a free slot?
//!
//! Counters live in memory as plain atomics, so a slow WAL write can never
//! hide a running job from the next selection.

use crate::{Host, Job};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Lease {
    pub host: Host,
    index: usize,
}

struct Slot {
    host: Host,
    running: AtomicUsize,
    down: AtomicBool,
}

/// Slot granted to a waiter: host index plus host, set once under the queue lock.
type Grant = Arc<(Mutex<Option<(usize, Host)>>, Condvar)>;

struct Waiter {
    priority: i32,
    seq: u64,
    capability: Option<String>,
    shared: Grant,
}

pub struct Scheduler {
    slots: Vec<Slot>,
    waiters: Mutex<Vec<Waiter>>,
    next_seq: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Scheduler {
    pub fn new(hosts: Vec<Host>) -> Scheduler {
        let slots = hosts
            .into_iter()
            .map(|host| Slot {
                host,
                running: AtomicUsize::new(0),
                down: AtomicBool::new(false),
            })
            .collect();
        Scheduler {
            slots,
            waiters: Mutex::new(Vec::new()),
            next_seq: AtomicU64::new(0),
        }
    }

    /// Reserve a slot on the eligible host with the lowest running/concurrency
    /// ratio, first in config order on ties.
    pub fn acquire(&self, job: &Job) -> Option<Lease> {
        loop {
            let index = self.pick_host(job.capability.as_deref())?;
            if self.try_reserve(index) {
                return Some(Lease {
                    host: self.slots[index].host.clone(),
                    index,
                });
            }
            // Lost the race for that slot: recompute on the next pass.
        }
    }

    /// Wait for a slot, honoring in-process priority (lower value first, FIFO
    /// on ties).
    ///
    /// Cross-process ordering needs the daemon: every Jellyfin job is its own
    /// process today, so this queue only orders jobs that share a process.
    pub fn acquire_blocking(&self, job: &Job, timeout: Duration) -> Option<Lease> {
        if let Some(lease) = self.acquire(job) {
            return Some(lease);
        }

        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new((Mutex::new(None), Condvar::new()));
        {
            let mut queue = lock(&self.waiters);
            let waiter = Waiter {
                priority: job.priority,
                seq,
                capability: job.capability.clone(),
                shared: Arc::clone(&shared),
            };
            let at = queue
                .iter()
                .position(|w| w.priority > waiter.priority)
                .unwrap_or(queue.len());
            queue.insert(at, waiter);
        }
        // A slot may have come free between our failed acquire and this
        // registration, so try to hand one out now.
        self.dispatch_to_waiters();

        let deadline = Instant::now() + timeout;
        {
            let (grant, cv) = &*shared;
            let mut guard = lock(grant);
            while guard.is_none() {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let (next, _) = cv
                    .wait_timeout(guard, deadline - now)
                    .unwrap_or_else(|e| e.into_inner());
                guard = next;
            }
        }

        if let Some((index, host)) = lock(&shared.0).take() {
            return Some(Lease { host, index });
        }

        // Not granted in time: withdraw. If the entry is already gone, a
        // grant raced us and we take it instead of dropping the slot.
        let mut queue = lock(&self.waiters);
        if let Some(pos) = queue.iter().position(|w| w.seq == seq) {
            queue.remove(pos);
            None
        } else {
            lock(&shared.0)
                .take()
                .map(|(index, host)| Lease { host, index })
        }
    }

    /// Free one slot. A second release on the same lease clamps at zero
    /// rather than underflowing the counter.
    pub fn release(&self, lease: Lease) {
        if let Some(slot) = self.slots.get(lease.index) {
            slot.running
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                    Some(v.saturating_sub(1))
                })
                .ok();
        }
        self.dispatch_to_waiters();
    }

    pub fn mark_down(&self, host: &str) {
        for slot in &self.slots {
            if slot.host.name == host {
                slot.down.store(true, Ordering::Release);
            }
        }
    }

    pub fn mark_up(&self, host: &str) {
        for slot in &self.slots {
            if slot.host.name == host {
                slot.down.store(false, Ordering::Release);
            }
        }
        self.dispatch_to_waiters();
    }

    /// Every configured worker, in config order.
    pub fn hosts(&self) -> Vec<Host> {
        self.slots.iter().map(|s| s.host.clone()).collect()
    }

    pub fn is_down(&self, host: &str) -> bool {
        self.slots
            .iter()
            .any(|s| s.host.name == host && s.down.load(Ordering::Acquire))
    }

    fn capability_ok(&self, index: usize, capability: Option<&str>) -> bool {
        let host = &self.slots[index].host;
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
        let mut best: Option<usize> = None;
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.down.load(Ordering::Acquire) {
                continue;
            }
            let running = slot.running.load(Ordering::Acquire);
            if running >= slot.host.concurrency {
                continue;
            }
            if !self.capability_ok(index, capability) {
                continue;
            }
            best = match best {
                None => Some(index),
                Some(current) => {
                    let cur = &self.slots[current];
                    let cur_running = cur.running.load(Ordering::Acquire);
                    let candidate = running as u128 * cur.host.concurrency as u128;
                    let incumbent = cur_running as u128 * slot.host.concurrency as u128;
                    if candidate < incumbent {
                        Some(index)
                    } else {
                        Some(current)
                    }
                }
            };
        }
        best
    }

    fn try_reserve(&self, index: usize) -> bool {
        let slot = &self.slots[index];
        slot.running
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                if v < slot.host.concurrency {
                    Some(v + 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    /// Hand free slots to queued waiters in priority order. A waiter whose
    /// capability no free host can serve is skipped, so a lower-priority
    /// waiter on another host still runs.
    fn dispatch_to_waiters(&self) {
        let mut queue = lock(&self.waiters);
        loop {
            if queue.is_empty() {
                return;
            }
            let mut grant: Option<(usize, usize)> = None;
            for (pos, waiter) in queue.iter().enumerate() {
                if let Some(index) = self.pick_host(waiter.capability.as_deref())
                    && self.try_reserve(index)
                {
                    grant = Some((pos, index));
                    break;
                }
            }
            let Some((pos, index)) = grant else {
                return;
            };
            let waiter = queue.remove(pos);
            let host = self.slots[index].host.clone();
            let (grant_slot, cv) = &*waiter.shared;
            *lock(grant_slot) = Some((index, host));
            cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Program;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    fn host(name: &str, concurrency: usize, capabilities: &[&str]) -> Host {
        Host {
            name: name.to_string(),
            concurrency,
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            ffmpeg_bin: None,
            ffprobe_bin: None,
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

    #[test]
    fn concurrency_cap_is_never_exceeded() {
        let scheduler = Arc::new(Scheduler::new(vec![host("w", 2, &[])]));
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
                scheduler.release(lease);
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread");
        }
        assert!(peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(held.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn capability_match_skips_the_wrong_host() {
        let scheduler = Scheduler::new(vec![
            host("vaapi", 1, &["vaapi"]),
            host("nvenc", 1, &["nvenc"]),
        ]);
        let lease = scheduler.acquire(&job(0, Some("nvenc"))).expect("a lease");
        assert_eq!(lease.host.name, "nvenc");
        scheduler.release(lease);

        // Empty capabilities on a host accept any job.
        let open = Scheduler::new(vec![host("open", 1, &[])]);
        let lease = open.acquire(&job(0, Some("qsv"))).expect("a lease");
        assert_eq!(lease.host.name, "open");
    }

    #[test]
    fn down_host_is_skipped_and_mark_up_restores_it() {
        let scheduler = Scheduler::new(vec![host("w", 1, &[])]);
        scheduler.mark_down("w");
        assert!(scheduler.is_down("w"));
        assert!(scheduler.acquire(&job(0, None)).is_none());

        scheduler.mark_up("w");
        assert!(!scheduler.is_down("w"));
        let lease = scheduler.acquire(&job(0, None)).expect("a lease");
        assert_eq!(lease.host.name, "w");
    }

    #[test]
    fn load_spreads_across_hosts_before_filling_one() {
        let scheduler = Scheduler::new(vec![host("a", 2, &[]), host("b", 2, &[])]);
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(scheduler.acquire(&job(0, None)).expect("a lease"));
        }
        let order: Vec<&str> = leases.iter().map(|l| l.host.name.as_str()).collect();
        assert_eq!(order, ["a", "b", "a", "b"]);
        assert!(scheduler.acquire(&job(0, None)).is_none());
        for lease in leases {
            scheduler.release(lease);
        }
    }

    #[test]
    fn blocking_acquire_times_out_then_wakes_on_release() {
        let scheduler = Arc::new(Scheduler::new(vec![host("w", 1, &[])]));
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
        scheduler.release(held);
        assert!(waiter.join().expect("waiter thread").is_some());
    }

    #[test]
    fn priority_wins_over_fifo() {
        let scheduler = Arc::new(Scheduler::new(vec![host("w", 1, &[])]));
        let held = scheduler.acquire(&job(0, None)).expect("the only slot");
        let (tx, rx) = std::sync::mpsc::channel();

        let low = {
            let scheduler = Arc::clone(&scheduler);
            let tx = tx.clone();
            thread::spawn(move || {
                let lease = scheduler.acquire_blocking(&job(0, None), Duration::from_secs(2));
                tx.send("low").ok();
                if let Some(lease) = lease {
                    scheduler.release(lease);
                }
            })
        };
        thread::sleep(Duration::from_millis(80));

        let high = {
            let scheduler = Arc::clone(&scheduler);
            let tx = tx.clone();
            thread::spawn(move || {
                let lease = scheduler.acquire_blocking(&job(-5, None), Duration::from_secs(2));
                tx.send("high").ok();
                if let Some(lease) = lease {
                    scheduler.release(lease);
                }
            })
        };
        thread::sleep(Duration::from_millis(80));

        scheduler.release(held);
        assert_eq!(rx.recv().expect("first grant"), "high");
        assert_eq!(rx.recv().expect("second grant"), "low");
        low.join().expect("low thread");
        high.join().expect("high thread");
    }
}
