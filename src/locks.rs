//! Cross-process placement state under `state_dir/coord/`.
//!
//! Jellyfin forks one rrffmpeg process per job, so an in-memory counter cannot
//! cap a host. Each slot is a file held by an exclusive `flock`: the kernel
//! drops the lock when the last descriptor closes, so SIGKILL leaves no stale
//! lock behind. Every failure here falls open: a broken state dir must never
//! stop a transcode.
//!
//! A dropped guard can still read as held for a moment: `fork` copies the
//! descriptor table, so a child keeps a duplicate of the locked descriptor
//! until its `exec`. Placement leans conservative when that happens, which
//! costs a retry and nothing else.

use crate::Host;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A slot this process holds. Dropping it closes the descriptor and frees the
/// lock, so no janitor has to clean up.
#[derive(Debug)]
pub struct SlotGuard {
    /// `None` means the state dir was unusable and this job runs uncapped.
    _file: Option<File>,
}

#[derive(Debug)]
pub struct LockDir {
    /// `state_dir/coord`. The journal lives beside it, not inside.
    root: PathBuf,
}

impl LockDir {
    /// `state_dir` is the journal directory; coordination files live under
    /// `coord/` inside it.
    pub fn new(state_dir: &Path) -> LockDir {
        LockDir {
            root: state_dir.join("coord"),
        }
    }

    /// The directory every coordination file shares. Clients must point at the
    /// same one for slots and weights to mean anything across machines.
    pub fn coord(&self) -> &std::path::Path {
        &self.root
    }

    /// Hold a free slot on `host`, or return `None` when another process holds
    /// them all. Slots are tried in index order; an unopenable slot file
    /// returns an uncapped guard instead of failing the job.
    pub fn try_acquire(&self, host: &Host, job: &str) -> Option<SlotGuard> {
        let dir = self.dir(&host.name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Some(self.uncapped(&dir, e));
        }
        for index in 0..host.concurrency {
            let path = self.slot_path(&host.name, index);
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
                Err(e) => return Some(self.uncapped(&path, e)),
            };
            match file.try_lock() {
                Ok(()) => {
                    // The content only helps a human reading `ls -l coord/*`.
                    // No decision ever reads it.
                    let _ = file.set_len(0);
                    let mut handle = &file;
                    let _ = handle.write_all(
                        format!("{} {} {}\n", std::process::id(), job, host.name).as_bytes(),
                    );
                    return Some(SlotGuard { _file: Some(file) });
                }
                Err(TryLockError::WouldBlock) => continue,
                Err(TryLockError::Error(e)) => return Some(self.uncapped(&path, e)),
            }
        }
        None
    }

    /// How many slots on `host` another process holds right now. An unreadable
    /// slot counts as no load, so placement still admits the job.
    pub fn occupancy(&self, host: &Host) -> usize {
        let mut held = 0;
        for index in 0..host.concurrency {
            let path = self.slot_path(&host.name, index);
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
                Err(TryLockError::WouldBlock) => held += 1,
                Err(TryLockError::Error(_)) => return 0,
            }
        }
        held
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

    fn slot_path(&self, host: &str, index: usize) -> PathBuf {
        self.dir(host).join(format!("slot.{index}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn host(name: &str, concurrency: usize) -> Host {
        Host {
            name: name.to_string(),
            concurrency,
            capabilities: Vec::new(),
            ffmpeg_bin: None,
            ffprobe_bin: None,
            agent_addr: None,
            ssh_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            paths: Vec::new(),
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
    fn a_second_guard_cannot_take_the_same_slot() {
        let (dir, locks) = scratch("busy");
        let host = host("worker", 1);
        let first = locks.try_acquire(&host, "job-a");
        assert!(first.is_some(), "the first guard must get the only slot");
        assert!(
            locks.try_acquire(&host, "job-b").is_none(),
            "the only slot was handed out twice"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn guards_on_one_host_take_different_slots() {
        let (dir, locks) = scratch("distinct");
        let host = host("worker", 2);
        let a = locks.try_acquire(&host, "job-a").expect("slot 0");
        // A second guard must land on slot 1, not retake slot 0.
        let b = locks.try_acquire(&host, "job-b").expect("slot 1");
        assert!(locks.try_acquire(&host, "job-c").is_none());
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropping_a_guard_frees_the_slot() {
        let (dir, locks) = scratch("free");
        let host = host("worker", 1);
        let first = locks.try_acquire(&host, "job-a").expect("the only slot");
        drop(first);
        assert!(
            eventually(|| locks.try_acquire(&host, "job-b").is_some()),
            "the slot stayed held after its guard dropped"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn occupancy_counts_held_guards() {
        let (dir, locks) = scratch("occupancy");
        let host = host("worker", 3);
        assert_eq!(locks.occupancy(&host), 0);
        let a = locks.try_acquire(&host, "job-a").expect("a slot");
        let b = locks.try_acquire(&host, "job-b").expect("another slot");
        assert_eq!(locks.occupancy(&host), 2);
        drop(a);
        assert!(
            eventually(|| locks.occupancy(&host) == 1),
            "occupancy did not drop after one guard was freed"
        );
        drop(b);
        assert!(
            eventually(|| locks.occupancy(&host) == 0),
            "occupancy did not reach zero after both guards were freed"
        );
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
            locks.try_acquire(&host, "job-a").is_some(),
            "a broken state dir must not stop a transcode"
        );
        assert_eq!(locks.occupancy(&host), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
