//! In-process repair: retest unhealthy workers, reap dead jobs, requeue.
//!
//! One thread, one cycle per `every`:
//!
//! 1. Reap: for each active job, ask the worker whether its process still runs.
//!    A job seen in two consecutive cycles that is reported dead gets a
//!    `Reaped` event. Reaping goes first because a stale active row is the
//!    defect that silently undercounts slots, and it uses no extra network
//!    round trip beyond the per-job liveness check.
//! 2. Reconcile: probe each configured host exactly once, then flip its flag
//!    and record the transition. Probing is the expensive round trip, so it is
//!    per host per cycle, never per job.
//!
//! The thread never exits on error and never panics; `stop` and `Drop` join it.
//! This replaces the external cron watchdog rffmpeg needed.

use crate::Host;
use crate::scheduler::Scheduler;
use crate::state::{Event, Journal};
use crate::transport::Transport;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct Janitor {
    stop_flag: Arc<AtomicBool>,
    /// Wakes the sleeper on `stop`, so a 1 hour interval does not hold up exit.
    wake: Arc<(Mutex<()>, Condvar)>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Janitor {
    pub fn spawn(
        scheduler: Arc<Scheduler>,
        journal: Arc<Journal>,
        transport: Arc<dyn Transport>,
        every: Duration,
    ) -> Janitor {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));

        let thread_flag = Arc::clone(&stop_flag);
        let thread_wake = Arc::clone(&wake);
        let handle = thread::Builder::new()
            .name("janitor".into())
            .spawn(move || {
                let mut seen: HashSet<String> = HashSet::new();
                let mut reaped: HashSet<String> = HashSet::new();
                while !thread_flag.load(Ordering::Relaxed) {
                    cycle(
                        &scheduler,
                        &journal,
                        transport.as_ref(),
                        &mut seen,
                        &mut reaped,
                    );
                    if thread_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    wait(&thread_flag, &thread_wake, every);
                }
            })
            .ok();

        Janitor {
            stop_flag,
            wake,
            handle: Mutex::new(handle),
        }
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.wake.1.notify_all();
        let handle = match self.handle.lock() {
            Ok(mut slot) => slot.take(),
            Err(poison) => poison.into_inner().take(),
        };
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for Janitor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One pass: reap orphans, then retest hosts.
fn cycle(
    scheduler: &Scheduler,
    journal: &Journal,
    transport: &dyn Transport,
    seen: &mut HashSet<String>,
    reaped: &mut HashSet<String>,
) {
    let hosts = scheduler.hosts();
    reap(journal, transport, &hosts, seen, reaped);
    reconcile(scheduler, journal, transport, &hosts);
}

fn reap(
    journal: &Journal,
    transport: &dyn Transport,
    hosts: &[Host],
    seen: &mut HashSet<String>,
    reaped: &mut HashSet<String>,
) {
    let active = journal.active_jobs();
    let current: HashSet<String> = active.iter().map(|(job, _)| job.clone()).collect();

    for (job, host_name) in &active {
        // `active_jobs` carries no start time, so age has to be inferred from
        // observation: a job is judged only after a previous cycle saw it. A
        // job started between the `Started` append and the remote pid file can
        // then never be reaped on its first cycle, which removes the race.
        if !seen.contains(job) || reaped.contains(job) {
            continue;
        }
        let Some(host) = hosts.iter().find(|host| &host.name == host_name) else {
            // A job on a host that is no longer configured cannot be checked.
            continue;
        };
        if !transport.job_alive(host, job) {
            journal.append(Event::Reaped { job: job.clone() });
            // The journal flush is async, so keep the id here to avoid sending
            // the same Reaped event again every cycle until it lands.
            reaped.insert(job.clone());
        }
    }

    reaped.retain(|job| current.contains(job));
    *seen = current;
}

fn reconcile(scheduler: &Scheduler, journal: &Journal, transport: &dyn Transport, hosts: &[Host]) {
    for host in hosts {
        // One probe per host per cycle. Never per job.
        let up = transport.probe(host);
        // The scheduler holds the flag placement reads; the journal records
        // transitions for the next start.
        let down = scheduler.is_down(&host.name);
        if down && up {
            scheduler.mark_up(&host.name);
            journal.append(Event::HostUp {
                host: host.name.clone(),
            });
        } else if !down && !up {
            scheduler.mark_down(&host.name);
            journal.append(Event::HostDown {
                host: host.name.clone(),
            });
        }
    }
}

/// Sleep for `total`, or less if `stop` is raised.
fn wait(stop: &AtomicBool, wake: &(Mutex<()>, Condvar), total: Duration) {
    let guard = match wake.0.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    };
    let _ = wake
        .1
        .wait_timeout_while(guard, total, |_| !stop.load(Ordering::Relaxed));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Event;
    use crate::{Error, Job, Result};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// Canned transport answers, one `Mutex` of state plus call counters.
    struct FakeTransport {
        answers: Mutex<(bool, bool)>,
        probes: AtomicUsize,
        alive_calls: AtomicUsize,
    }

    impl FakeTransport {
        fn new(probe: bool, alive: bool) -> FakeTransport {
            FakeTransport {
                answers: Mutex::new((probe, alive)),
                probes: AtomicUsize::new(0),
                alive_calls: AtomicUsize::new(0),
            }
        }
    }

    impl Transport for FakeTransport {
        fn run(&self, _host: &Host, _job: &Job, _stderr_as_stdout: bool) -> Result<i32> {
            Err(Error::Transport("fake transport".into()))
        }

        fn probe(&self, _host: &Host) -> bool {
            self.probes.fetch_add(1, Ordering::SeqCst);
            self.answers
                .lock()
                .map(|answers| answers.0)
                .unwrap_or(false)
        }

        fn job_alive(&self, _host: &Host, _job_id: &str) -> bool {
            self.alive_calls.fetch_add(1, Ordering::SeqCst);
            self.answers
                .lock()
                .map(|answers| answers.1)
                .unwrap_or(false)
        }
    }

    fn host(name: &str) -> Host {
        Host {
            name: name.to_string(),
            concurrency: 1,
            capabilities: Vec::new(),
            paths: Vec::new(),
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rrffmpeg-janitor-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Poll until `cond` holds or the deadline passes. Never sleeps blindly.
    fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn start(journal: &Journal, job: &str, host_name: &str) -> bool {
        journal.append(Event::Started {
            job: job.to_string(),
            host: host_name.to_string(),
        });
        journal
            .active_jobs()
            .iter()
            .any(|(active, _)| active == job)
    }

    #[test]
    fn a_dead_job_is_reaped_and_leaves_the_active_list() {
        let dir = tempdir("reap");
        let journal = Arc::new(Journal::open(&dir).expect("journal"));
        assert!(
            start(&journal, "j1", "virmire"),
            "the journal does not report the Started job as active"
        );
        let scheduler = Arc::new(Scheduler::new(vec![host("virmire")]));
        let transport = Arc::new(FakeTransport::new(true, false));

        let janitor = Janitor::spawn(
            scheduler,
            Arc::clone(&journal),
            transport,
            Duration::from_millis(10),
        );

        let gone = wait_until(|| {
            !journal
                .active_jobs()
                .iter()
                .any(|(active, _)| active == "j1")
        });
        janitor.stop();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the dead job was still active after the deadline");
    }

    #[test]
    fn an_alive_job_stays_active() {
        let dir = tempdir("alive");
        let journal = Arc::new(Journal::open(&dir).expect("journal"));
        assert!(
            start(&journal, "j1", "virmire"),
            "the journal does not report the Started job as active"
        );
        let scheduler = Arc::new(Scheduler::new(vec![host("virmire")]));
        let transport = Arc::new(FakeTransport::new(true, true));

        let janitor = Janitor::spawn(
            scheduler,
            Arc::clone(&journal),
            transport.clone(),
            Duration::from_millis(10),
        );

        // Let several cycles run, then check the job survived all of them.
        let polled = wait_until(|| transport.alive_calls.load(Ordering::SeqCst) >= 3);
        let still_active = journal
            .active_jobs()
            .iter()
            .any(|(active, _)| active == "j1");
        janitor.stop();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(polled, "the janitor never checked the job");
        assert!(
            still_active,
            "an alive job was removed from the active list"
        );
    }

    #[test]
    fn a_down_host_that_probes_true_is_marked_up() {
        let dir = tempdir("host-up");
        let journal = Arc::new(Journal::open(&dir).expect("journal"));
        let scheduler = Arc::new(Scheduler::new(vec![host("virmire")]));
        scheduler.mark_down("virmire");
        let transport = Arc::new(FakeTransport::new(true, true));

        let janitor = Janitor::spawn(
            scheduler.clone(),
            journal,
            transport.clone(),
            Duration::from_millis(10),
        );

        let ok = wait_until(|| {
            transport.probes.load(Ordering::SeqCst) >= 1 && !scheduler.is_down("virmire")
        });
        janitor.stop();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!scheduler.is_down("virmire"), "the host is still down");
        assert!(ok, "the host was never probed");
    }

    #[test]
    fn a_live_host_whose_probe_fails_is_marked_down() {
        let dir = tempdir("host-down");
        let journal = Arc::new(Journal::open(&dir).expect("journal"));
        let scheduler = Arc::new(Scheduler::new(vec![host("virmire")]));
        let transport = Arc::new(FakeTransport::new(false, true));

        let janitor = Janitor::spawn(
            scheduler.clone(),
            journal,
            transport.clone(),
            Duration::from_millis(10),
        );

        let ok = wait_until(|| {
            transport.probes.load(Ordering::SeqCst) >= 1 && scheduler.is_down("virmire")
        });
        janitor.stop();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok, "the failing host was never marked down");
        assert!(scheduler.is_down("virmire"), "the host is still marked up");
    }

    #[test]
    fn stop_returns_promptly_with_a_long_interval() {
        let dir = tempdir("stop");
        let journal = Arc::new(Journal::open(&dir).expect("journal"));
        let scheduler = Arc::new(Scheduler::new(vec![host("virmire")]));
        let transport = Arc::new(FakeTransport::new(true, true));

        let janitor = Janitor::spawn(scheduler, journal, transport, Duration::from_secs(3600));

        let began = Instant::now();
        janitor.stop();
        let elapsed = began.elapsed();
        // A second stop is a no-op, and Drop must not hang either.
        janitor.stop();
        drop(janitor);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            elapsed < Duration::from_millis(200),
            "stop took {elapsed:?} with a 1 hour interval"
        );
    }
}
