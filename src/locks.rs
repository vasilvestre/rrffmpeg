//! Cross-process placement state under `state_dir/coord/`.
//!
//! Jellyfin forks one rrffmpeg process per job, so an in-memory counter cannot
//! cap a host. Each slot is a file held by an exclusive `flock`: the kernel
//! drops the lock when the last descriptor closes, so SIGKILL leaves no stale
//! lock behind. Every failure here falls open: a broken state dir must never
//! stop a transcode.
//!
//! Each host has two pools. `slot.N` caps Heavy jobs, `light.N` caps Light ones
//! (ADR 0032). They are separate files, so a probe never waits for a transcode
//! to release anything.
//!
//! A dropped guard can still read as held for a moment: `fork` copies the
//! descriptor table, so a child keeps a duplicate of the locked descriptor
//! until its `exec`. Placement leans conservative when that happens, which
//! costs a retry and nothing else.

use crate::{Class, Host};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A slot this process holds. Dropping it closes the descriptor and frees the
/// lock, so no janitor has to clean up.
#[derive(Debug)]
pub struct SlotGuard {
    /// `None` means the state dir was unusable and this job runs uncapped.
    _file: Option<File>,
}

/// The word a parked job writes in its own slot file. Placement discounts a
/// slot that carries it, and nothing else reads it.
const PARKED: &str = "parked";

#[derive(Debug)]
pub struct LockDir {
    /// `state_dir/coord`. The journal lives beside it, not inside.
    root: PathBuf,
    /// The most jobs a pool may ever hold. A parked job does not count, so a
    /// spare file is allowed beyond the cap, up to this bound. It is this
    /// machine's core count: right for a local host, and a safe bound for a
    /// worker whose cores are unknown (ADR 0033).
    ceiling: usize,
}

/// One attempt at one slot file.
enum Attempt {
    Guard(SlotGuard),
    Busy,
    Broken(PathBuf, std::io::Error),
}

/// Whether the slot file's holder marked itself parked. Reading is allowed
/// even while another process holds the lock.
fn is_parked(file: &File) -> bool {
    let mut text = String::new();
    let mut handle = file;
    if handle.seek(SeekFrom::Start(0)).is_err() {
        return false;
    }
    if handle.read_to_string(&mut text).is_err() {
        return false;
    }
    text.split_whitespace().last() == Some(PARKED)
}

impl LockDir {
    /// `state_dir` is the journal directory; coordination files live under
    /// `coord/` inside it.
    pub fn new(state_dir: &Path) -> LockDir {
        LockDir {
            root: state_dir.join("coord"),
            ceiling: crate::sizing::ceiling(crate::config::cpus()),
        }
    }

    /// The directory every coordination file shares. Clients must point at the
    /// same one for slots and weights to mean anything across machines.
    pub fn coord(&self) -> &std::path::Path {
        &self.root
    }

    /// Hold a free slot in `class`'s pool on `host`, or return `None` when
    /// another process holds them all. Slots are tried in index order; an
    /// unopenable slot file returns an uncapped guard instead of failing the
    /// job.
    pub fn try_acquire(&self, host: &Host, job: &str, class: Class) -> Option<SlotGuard> {
        let dir = self.dir(&host.name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Some(self.uncapped(&dir, e));
        }
        let slots = host.slots(class);
        for index in 0..slots {
            match self.take(host, job, class, index) {
                Attempt::Guard(guard) => return Some(guard),
                Attempt::Busy => continue,
                Attempt::Broken(path, e) => return Some(self.uncapped(&path, e)),
            }
        }
        // Every slot of the pool is held. A parked job holds its file and
        // feeds nobody, so one spare is allowed per parked job, and the pool
        // never passes the ceiling (ADR 0033).
        let spare = self.parked_in(host, class, slots).min(self.ceiling);
        for index in slots..slots + spare {
            if let Attempt::Guard(guard) = self.take(host, job, class, index) {
                return Some(guard);
            }
        }
        None
    }

    /// Hold slot `index`, or say why not. The single place a slot is taken,
    /// so the cap and the spare files cannot drift apart.
    fn take(&self, host: &Host, job: &str, class: Class, index: usize) -> Attempt {
        let path = self.slot_path(&host.name, class, index);
        // read+write+create: std sets O_CLOEXEC, so the ssh child cannot
        // inherit the fd and pin the slot after the client dies.
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) => return Attempt::Broken(path, e),
        };
        match file.try_lock() {
            Ok(()) => {
                // The content tells a human reading `ls -l coord/*` and the
                // janitor which pid holds the slot. The last word is the only
                // thing placement reads: `parked`, written by the job itself.
                let _ = file.set_len(0);
                let mut handle = &file;
                let _ = handle.write_all(
                    format!(
                        "{} {} {} {}\n",
                        std::process::id(),
                        job,
                        host.name,
                        class.name()
                    )
                    .as_bytes(),
                );
                Attempt::Guard(SlotGuard { _file: Some(file) })
            }
            Err(TryLockError::WouldBlock) => Attempt::Busy,
            Err(TryLockError::Error(e)) => Attempt::Broken(path, e),
        }
    }

    /// Slot files of this pool that another process marked as parked.
    fn parked_in(&self, host: &Host, class: Class, slots: usize) -> usize {
        let mut parked = 0;
        for index in 0..slots {
            let path = self.slot_path(&host.name, class, index);
            let Ok(file) = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
            else {
                return 0;
            };
            if file.try_lock_shared().is_err() && is_parked(&file) {
                parked += 1;
            }
        }
        parked
    }

    /// Write or clear the parked word in the slot file this process holds for
    /// `job` on `host`. The job's own process is the only writer: it holds the
    /// lock. The job name matters because one process can hold more than one
    /// file: a spare admission sits beside the capped ones.
    pub fn mark_parked(&self, host_name: &str, job: &str, parked: bool) -> bool {
        let Ok(entries) = std::fs::read_dir(self.dir(host_name)) else {
            return false;
        };
        let pid = std::process::id();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("slot.") || name.starts_with("light.")) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let mut words: Vec<&str> = text.split_whitespace().collect();
            if words.first().and_then(|word| word.parse::<u32>().ok()) != Some(pid) {
                continue;
            }
            if words.get(1).copied() != Some(job) {
                continue;
            }
            words.truncate(4);
            let mut line = words.join(" ");
            if parked {
                line.push(' ');
                line.push_str(PARKED);
            }
            line.push('\n');
            return std::fs::write(entry.path(), line).is_ok();
        }
        false
    }

    /// How many slots in `class`'s pool on `host` another process holds right
    /// now. An unreadable slot counts as no load, so placement still admits the
    /// job.
    pub fn occupancy(&self, host: &Host, class: Class) -> usize {
        let mut held = 0;
        for index in 0..host.slots(class) {
            let path = self.slot_path(&host.name, class, index);
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
            {
                Ok(file) => file,
                Err(_) => return 0,
            };
            // A shared probe never conflicts with another probe, so two
            // threads reading occupancy at once do not count each other as
            // load. An exclusive holder still blocks it.
            match file.try_lock_shared() {
                Ok(()) => drop(file),
                // A parked job still holds its file, but it is waiting on a
                // viewer, not encoding, so it does not count as load.
                Err(TryLockError::WouldBlock) => {
                    if !is_parked(&file) {
                        held += 1;
                    }
                }
                Err(TryLockError::Error(_)) => return 0,
            }
        }
        held
    }

    /// Clear every slot of `host` whose recorded pid is gone, in both pools.
    /// The flock is already free by then, so this removes the line a killed
    /// client left in `coord/*/slot.*` and hands the slot back with a clean
    /// file. A slot a live process still holds is left alone.
    pub fn reap_slots(&self, host: &Host) -> usize {
        let mut cleared = 0;
        for class in [Class::Heavy, Class::Light] {
            for index in 0..host.slots(class) {
                let path = self.slot_path(&host.name, class, index);
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(pid) = text.split(' ').next().and_then(|p| p.parse::<i32>().ok()) else {
                    continue;
                };
                if pid_alive(pid) {
                    continue;
                }
                let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
                    continue;
                };
                if file.try_lock().is_ok() && file.set_len(0).is_ok() {
                    cleared += 1;
                }
            }
        }
        cleared
    }

    /// Publish "host is down" for `ttl`. Every other process trusts the verdict
    /// only until the expiry, which is the whole repair mechanism.
    pub fn mark_down(&self, host: &str, ttl: Duration) {
        let dir = self.dir(host);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("rrffmpeg: mark {host} down: {e}");
            return;
        }
        let expiry = now_millis() + ttl.as_millis() as u64;
        // Write then rename, so a reader never parses half a number.
        let temp = dir.join(format!("down.{}.tmp", std::process::id()));
        if let Err(e) = std::fs::write(&temp, expiry.to_string()) {
            eprintln!("rrffmpeg: mark {host} down: {e}");
            return;
        }
        if let Err(e) = std::fs::rename(&temp, self.down_path(host)) {
            eprintln!("rrffmpeg: mark {host} down: {e}");
            let _ = std::fs::remove_file(&temp);
        }
    }

    /// Clear the shared "host down" verdict.
    pub fn mark_up(&self, host: &str) {
        let _ = std::fs::remove_file(self.down_path(host));
    }

    /// True while a live verdict says `host` is down. An expired or unreadable
    /// file reads as up, and the expired file is removed.
    pub fn is_down(&self, host: &str) -> bool {
        let path = self.down_path(host);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return false;
        };
        let Ok(expiry) = text.trim().parse::<u64>() else {
            let _ = std::fs::remove_file(&path);
            return false;
        };
        if now_millis() >= expiry {
            let _ = std::fs::remove_file(&path);
            return false;
        }
        true
    }

    fn dir(&self, host: &str) -> PathBuf {
        self.root.join(hostkey(host))
    }

    fn slot_path(&self, host: &str, class: Class, index: usize) -> PathBuf {
        let name = match class {
            Class::Heavy => format!("slot.{index}"),
            Class::Light => format!("light.{index}"),
        };
        self.dir(host).join(name)
    }

    fn down_path(&self, host: &str) -> PathBuf {
        self.dir(host).join("down")
    }

    fn uncapped(&self, path: &Path, e: std::io::Error) -> SlotGuard {
        eprintln!("rrffmpeg: lock {}: {e}, running uncapped", path.display());
        SlotGuard { _file: None }
    }
}

/// FNV-1a 64 of the host name. The name is an ssh destination and can contain
/// `@` or `:`, so it never becomes a path component itself.
pub(crate) fn hostkey(name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Does this pid still exist? Signal 0 asks the kernel and delivers nothing.
/// A pid of zero means "every process in my group", so it is never a job.
pub(crate) fn pid_alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn host(name: &str, concurrency: usize) -> Host {
        Host {
            local: false,
            name: name.to_string(),
            concurrency,
            light_concurrency: 2,
            capabilities: Vec::new(),
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

    /// Poll until `cond` holds. A slot freed in this process can still look
    /// held for the moment a forked child needs to reach its `exec`.
    fn eventually(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn scratch(tag: &str) -> (PathBuf, LockDir) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rrffmpeg-locks-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let locks = LockDir::new(&dir);
        (dir, locks)
    }

    #[test]
    fn a_parked_job_holds_its_file_but_stops_counting_as_load() {
        let (dir, locks) = scratch("parked");
        let host = host("worker", 1);
        let _first = locks
            .try_acquire(&host, "job-a", Class::Heavy)
            .expect("the only slot");
        assert_eq!(locks.occupancy(&host, Class::Heavy), 1);
        assert!(
            locks.mark_parked("worker", "job-a", true),
            "the holder marks itself"
        );
        assert_eq!(
            locks.occupancy(&host, Class::Heavy),
            0,
            "a job waiting on a viewer is not load"
        );
        // One spare file, because one parked job freed the count. A second
        // admission finds the spare taken.
        let spare = locks.try_acquire(&host, "job-b", Class::Heavy);
        assert!(spare.is_some(), "a parked slot admits one more job");
        assert!(
            locks.try_acquire(&host, "job-c", Class::Heavy).is_none(),
            "the spare is not a second pool"
        );
        // The job takes the CPU again: it counts, and needs no new slot.
        assert!(locks.mark_parked("worker", "job-a", false));
        assert_eq!(locks.occupancy(&host, Class::Heavy), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_guard_cannot_take_the_same_slot() {
        let (dir, locks) = scratch("busy");
        let host = host("worker", 1);
        let first = locks.try_acquire(&host, "job-a", Class::Heavy);
        assert!(first.is_some(), "the first guard must get the only slot");
        assert!(
            locks.try_acquire(&host, "job-b", Class::Heavy).is_none(),
            "the only slot was handed out twice"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn guards_on_one_host_take_different_slots() {
        let (dir, locks) = scratch("distinct");
        let host = host("worker", 2);
        let a = locks
            .try_acquire(&host, "job-a", Class::Heavy)
            .expect("slot 0");
        // A second guard must land on slot 1, not retake slot 0.
        let b = locks
            .try_acquire(&host, "job-b", Class::Heavy)
            .expect("slot 1");
        assert!(locks.try_acquire(&host, "job-c", Class::Heavy).is_none());
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The whole point of two pools: a Light job takes its own file, so a
    /// transcode on the only heavy slot never costs a probe a wait.
    #[test]
    fn a_light_slot_is_independent_of_the_heavy_pool() {
        let (dir, locks) = scratch("light-pool");
        let host = host("worker", 1);
        let heavy = locks
            .try_acquire(&host, "transcode", Class::Heavy)
            .expect("the heavy slot");
        assert!(
            locks
                .try_acquire(&host, "transcode-2", Class::Heavy)
                .is_none()
        );

        let a = locks
            .try_acquire(&host, "probe", Class::Light)
            .expect("a light slot");
        let b = locks
            .try_acquire(&host, "thumb", Class::Light)
            .expect("the second light slot");
        assert!(
            locks.try_acquire(&host, "thumb-2", Class::Light).is_none(),
            "the light pool is capped at light_concurrency"
        );
        assert_eq!(locks.occupancy(&host, Class::Heavy), 1);
        assert_eq!(locks.occupancy(&host, Class::Light), 2);
        // The two pools are separate files, so a reader can tell them apart.
        let dirs = locks.dir("worker");
        assert!(dirs.join("slot.0").exists());
        assert!(dirs.join("light.0").exists() && dirs.join("light.1").exists());

        drop((heavy, a, b));
        assert!(
            eventually(|| locks.occupancy(&host, Class::Light) == 0),
            "the light pool did not drain"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropping_a_guard_frees_the_slot() {
        let (dir, locks) = scratch("free");
        let host = host("worker", 1);
        let first = locks
            .try_acquire(&host, "job-a", Class::Heavy)
            .expect("the only slot");
        drop(first);
        assert!(
            eventually(|| locks.try_acquire(&host, "job-b", Class::Heavy).is_some()),
            "the slot stayed held after its guard dropped"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn occupancy_counts_held_guards() {
        let (dir, locks) = scratch("occupancy");
        let host = host("worker", 3);
        assert_eq!(locks.occupancy(&host, Class::Heavy), 0);
        let a = locks
            .try_acquire(&host, "job-a", Class::Heavy)
            .expect("a slot");
        let b = locks
            .try_acquire(&host, "job-b", Class::Heavy)
            .expect("another slot");
        assert_eq!(locks.occupancy(&host, Class::Heavy), 2);
        drop(a);
        assert!(
            eventually(|| locks.occupancy(&host, Class::Heavy) == 1),
            "occupancy did not drop after one guard was freed"
        );
        drop(b);
        assert!(
            eventually(|| locks.occupancy(&host, Class::Heavy) == 0),
            "occupancy did not reach zero after both guards were freed"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A killed client leaves its pid in the slot file. The flock is already
    /// free, so the janitor can clear the line and hand the slot back clean.
    #[test]
    fn a_slot_whose_pid_is_gone_is_reaped() {
        let (dir, locks) = scratch("reap-slot");
        let host = host("worker", 1);
        let host_dir = locks.dir("worker");
        std::fs::create_dir_all(&host_dir).expect("host dir");
        // A pid the kernel cannot have handed out, in the heavy slot.
        std::fs::write(host_dir.join("slot.0"), "2147483647 gone worker heavy\n").expect("stale");
        // A live pid in a light slot must survive the same pass.
        std::fs::write(
            host_dir.join("light.0"),
            format!("{} running worker light\n", std::process::id()),
        )
        .expect("live");

        assert_eq!(locks.reap_slots(&host), 1);
        assert_eq!(
            std::fs::read_to_string(host_dir.join("slot.0")).unwrap(),
            ""
        );
        assert!(
            std::fs::read_to_string(host_dir.join("light.0"))
                .unwrap()
                .contains("running")
        );

        // A slot a live process holds is left alone even with a dead pid in it.
        let held = locks
            .try_acquire(&host, "holder", Class::Heavy)
            .expect("the heavy slot");
        std::fs::write(host_dir.join("slot.0"), "2147483647 ghost worker heavy\n").expect("stale");
        assert_eq!(locks.reap_slots(&host), 0, "a held slot is not ours");
        drop(held);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_down_verdict_expires_on_its_own() {
        let (dir, locks) = scratch("down");
        locks.mark_down("worker", Duration::from_millis(100));
        assert!(locks.is_down("worker"));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!locks.is_down("worker"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mark_up_clears_the_verdict() {
        let (dir, locks) = scratch("up");
        locks.mark_down("worker", Duration::from_secs(60));
        locks.mark_up("worker");
        assert!(!locks.is_down("worker"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_verdict_file_with_garbage_is_ignored() {
        let (dir, locks) = scratch("garbage");
        let host_dir = locks.dir("worker");
        std::fs::create_dir_all(&host_dir).expect("host dir");
        let path = locks.down_path("worker");
        std::fs::write(&path, "not a number").expect("write verdict");
        assert!(!locks.is_down("worker"), "garbage must read as up");
        assert!(!path.exists(), "the unreadable verdict must be removed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unwritable_root_fails_open() {
        let (dir, _) = scratch("unwritable");
        // A regular file where the state dir should be makes create_dir_all fail.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a dir").expect("write blocker");
        let locks = LockDir::new(&blocker);
        let host = host("worker", 1);
        assert!(
            locks.try_acquire(&host, "job-a", Class::Heavy).is_some(),
            "a broken state dir must not stop a transcode"
        );
        assert_eq!(locks.occupancy(&host, Class::Heavy), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
