//! Who goes first when jobs wait for a slot.
//!
//! Jellyfin forks one process per job and several clients can share a worker,
//! so the order has to live on disk next to the slot files. Each waiting job
//! publishes one ticket under `coord/<host>/queue/` and refreshes it while it
//! waits. Before taking a free slot it checks that no better ticket waits.
//! Nothing enforces the order: every waiter declines the slot on its own, which
//! is why the mechanism survives a killed process and needs no daemon.
//!
//! Rank is class first, so a probe beats a transcode inside one client. Then it
//! is the client's share of recent starts, `starts / weight`. Two clients that
//! both wait get starts in the ratio of their weights; a client alone still
//! takes every slot, because nothing outranks it.

use crate::locks::hostkey;
use std::collections::HashMap;
use std::fs::{File, FileTimes, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A ticket nobody refreshed for this long belongs to a dead or finished job.
/// Waiters refresh every poll, at most 400 ms apart.
const STALE_AFTER: Duration = Duration::from_secs(3);
/// Start counts older than this say nothing about the current load. Expiry is
/// what keeps a client that ran alone for a month from starving the next one.
const SERVED_TTL: Duration = Duration::from_secs(60);

/// One waiting job. Dropping the lane removes its ticket.
#[derive(Debug)]
pub struct Lane {
    /// `None` means the queue directory is unusable: the job runs unqueued.
    file: Option<File>,
    dir: PathBuf,
    path: PathBuf,
    class: i32,
    weight: u32,
    seq: u64,
    client: String,
}

impl Drop for Lane {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The shared queue under `state_dir/coord`. Every client that points at the
/// same `state_dir` sees the same queue, which is what makes the weight work
/// across clients.
#[derive(Debug)]
pub struct Queue {
    coord: PathBuf,
}

impl Queue {
    pub fn new(coord: &Path) -> Queue {
        Queue {
            coord: coord.to_path_buf(),
        }
    }

    /// Publish a ticket and return the handle that keeps it alive.
    pub fn join(&self, host: &str, class: i32, client: &str, weight: u32) -> Lane {
        let dir = self.dir(host);
        let seq = now_nanos();
        let path = dir.join(format!("{seq}-{}.want", std::process::id()));
        let mut lane = Lane {
            file: None,
            dir,
            path,
            class,
            weight,
            seq,
            client: client.to_string(),
        };
        if let Err(e) = std::fs::create_dir_all(&lane.dir) {
            eprintln!(
                "rrffmpeg: queue {}: {e}, waiting unqueued",
                lane.dir.display()
            );
            return lane;
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lane.path)
        {
            Ok(mut file) => {
                let body = format!("{class} {weight} {seq} {client}\n");
                if let Err(e) = file.write_all(body.as_bytes()) {
                    eprintln!(
                        "rrffmpeg: queue {}: {e}, waiting unqueued",
                        lane.path.display()
                    );
                    let _ = std::fs::remove_file(&lane.path);
                    return lane;
                }
                lane.file = Some(file);
                lane
            }
            Err(e) => {
                eprintln!(
                    "rrffmpeg: queue {}: {e}, waiting unqueued",
                    lane.path.display()
                );
                lane
            }
        }
    }

    /// Say "still waiting" without rewriting the file. A waiter that stops
    /// refreshing is ignored after `STALE_AFTER`.
    pub fn refresh(&self, lane: &Lane) {
        if let Some(file) = &lane.file {
            let now = FileTimes::new().set_modified(SystemTime::now());
            let _ = file.set_times(now);
        }
    }

    /// True when a live job waits in this host's lane.
    ///
    /// A job that can take a slot without queueing still has to ask, or every
    /// newcomer jumps the queue a waiter is standing in, and the waiter sleeps
    /// through slot after slot.
    pub fn has_waiters(&self, host: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(self.dir(host)) else {
            return false;
        };
        entries.flatten().any(|entry| {
            entry.path().extension().and_then(|e| e.to_str()) == Some("want") && !stale(&entry)
        })
    }

    /// True when no other ticket ranks before this one.
    pub fn ahead(&self, lane: &Lane) -> bool {
        self.before_me(lane) == 0
    }

    /// How many live tickets rank before this one.
    ///
    /// A waiter may pass as soon as that count is under the number of free
    /// slots: strict order among the slots, not one job per poll.
    pub fn before_me(&self, lane: &Lane) -> usize {
        if lane.file.is_none() {
            return 0;
        }
        let served = self.starts(&lane.dir);
        let mine = rank(
            lane.class,
            lane.weight,
            &lane.client,
            lane.seq,
            &lane.path,
            &served,
        );
        let entries = match std::fs::read_dir(&lane.dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        let mut before = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path == lane.path || path.extension().and_then(|e| e.to_str()) != Some("want") {
                continue;
            }
            if stale(&entry) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some((class, weight, seq, client)) = parse(&text, &path) else {
                continue;
            };
            if rank(class, weight, &client, seq, &path, &served).before(&mine) {
                before += 1;
            }
        }
        before
    }

    /// Count one start for `client` on `host`. Called only when the job had to
    /// queue, so a client that is alone never builds a lead.
    pub fn charge(&self, host: &str, client: &str) {
        let dir = self.dir(host);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let lock_path = dir.join("served.lock");
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path);
        let Ok(lock) = lock else { return };
        if lock.lock().is_err() {
            return;
        }
        let mut starts = self.starts(&dir);
        *starts.entry(client.to_string()).or_insert(0) += 1;
        let mut body = String::new();
        for (name, count) in &starts {
            body.push_str(&format!("{count} {name}\n"));
        }
        let temp = dir.join(format!("served.{}.tmp", std::process::id()));
        if std::fs::write(&temp, body).is_ok() {
            let _ = std::fs::rename(&temp, dir.join("served"));
        }
    }

    /// Starts per client, empty once the file is older than `SERVED_TTL`.
    fn starts(&self, dir: &Path) -> Starts {
        let path = dir.join("served");
        let Ok(meta) = std::fs::metadata(&path) else {
            return Starts::new();
        };
        if age(meta.modified().ok()) > SERVED_TTL {
            return Starts::new();
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Starts::new();
        };
        let mut out = Starts::new();
        for line in text.lines() {
            let Some((count, client)) = line.split_once(' ') else {
                continue;
            };
            if let Ok(count) = count.parse::<u64>() {
                out.insert(client.to_string(), count);
            }
        }
        out
    }

    fn dir(&self, host: &str) -> PathBuf {
        self.coord.join(hostkey(host)).join("queue")
    }
}

type Starts = HashMap<String, u64>;

/// One ticket, parsed. `path` breaks ties so two waiters never both think they
/// are first.
struct Rank {
    class: i32,
    weight: u32,
    starts: u64,
    seq: u64,
    name: String,
}

impl Rank {
    fn before(&self, other: &Rank) -> bool {
        if self.class != other.class {
            return self.class < other.class;
        }
        // starts / weight, compared without floating point.
        let left = u128::from(self.starts) * u128::from(other.weight);
        let right = u128::from(other.starts) * u128::from(self.weight);
        if left != right {
            return left < right;
        }
        if self.seq != other.seq {
            return self.seq < other.seq;
        }
        self.name < other.name
    }
}

fn rank(class: i32, weight: u32, client: &str, seq: u64, path: &Path, served: &Starts) -> Rank {
    Rank {
        class,
        weight: weight.max(1),
        starts: *served.get(client).unwrap_or(&0),
        seq,
        name: path.to_string_lossy().into_owned(),
    }
}

/// `class weight seq client`. The client comes last so a name with a space
/// still parses.
fn parse(text: &str, path: &Path) -> Option<(i32, u32, u64, String)> {
    let mut fields = text.trim_end().splitn(4, ' ');
    let class = fields.next()?.parse().ok()?;
    let weight = fields.next()?.parse().ok()?;
    let seq = fields.next()?.parse().ok()?;
    let client = fields.next()?.trim().to_string();
    if client.is_empty() {
        eprintln!("rrffmpeg: queue {}: no client name", path.display());
        return None;
    }
    Some((class, weight, seq, client))
}

fn stale(entry: &std::fs::DirEntry) -> bool {
    let Ok(meta) = entry.metadata() else {
        return false;
    };
    age(meta.modified().ok()) > STALE_AFTER
}

fn age(modified: Option<SystemTime>) -> Duration {
    modified
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .unwrap_or_default()
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch(tag: &str) -> (PathBuf, Queue) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rrffmpeg-queue-{}-{n}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        (dir.clone(), Queue::new(&dir))
    }

    #[test]
    fn a_probe_goes_before_a_transcode() {
        let (dir, queue) = scratch("class");
        let transcode = queue.join("w", 0, "alice", 100);
        let probe = queue.join("w", -5, "alice", 100);
        assert!(queue.ahead(&probe), "the probe must rank first");
        assert!(!queue.ahead(&transcode), "the transcode must yield");
        drop(probe);
        assert!(queue.ahead(&transcode), "the transcode is alone now");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_client_with_fewer_starts_per_weight_goes_first() {
        let (dir, queue) = scratch("weight");
        queue.charge("w", "alice");
        queue.charge("w", "alice");
        // Same weight, zero starts: bob is next.
        let alice = queue.join("w", 0, "alice", 100);
        let bob = queue.join("w", 0, "bob", 100);
        assert!(queue.ahead(&bob));
        assert!(!queue.ahead(&alice));
        drop((alice, bob));

        // 1/400 is less than 0/100? No: zero starts always comes first, so the
        // heavy client with one start yields to the idle one.
        queue.charge("w", "carol");
        let carol = queue.join("w", 0, "carol", 400);
        let dave = queue.join("w", 0, "dave", 100);
        assert!(queue.ahead(&dave));
        assert!(!queue.ahead(&carol));
        drop((carol, dave));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn weight_decides_between_clients_that_both_started() {
        let (dir, queue) = scratch("share");
        // Three starts each: carol at weight 400 has 3/400 of her share used,
        // dave at weight 100 has 3/100, so dave is four times over his.
        for _ in 0..3 {
            queue.charge("w", "carol");
            queue.charge("w", "dave");
        }
        let carol = queue.join("w", 0, "carol", 400);
        let dave = queue.join("w", 0, "dave", 100);
        assert!(
            queue.ahead(&carol),
            "carol is behind her share, she goes first"
        );
        assert!(!queue.ahead(&dave), "dave is over his share, he waits");
        drop((carol, dave));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rank_compares_starts_over_weight_without_floating_point() {
        let path = Path::new("t");
        let none = Starts::new();
        let heavy_idle = rank(0, 400, "a", 0, path, &none);
        let light_busy = rank(0, 100, "b", 1, path, &none);
        assert!(
            heavy_idle.before(&light_busy),
            "no start beats a busy client"
        );

        // 4/400 == 1/100: the share is level, so arrival order decides.
        let mut four = Starts::new();
        four.insert("a".to_string(), 4);
        let mut one = Starts::new();
        one.insert("b".to_string(), 1);
        let heavy_level = rank(0, 400, "a", 10, path, &four);
        let light_level = rank(0, 100, "b", 20, path, &one);
        assert!(
            heavy_level.before(&light_level),
            "level share, earlier arrival wins"
        );
        assert!(!light_level.before(&heavy_level));

        // One extra start at weight 400 is still behind one extra start at
        // weight 100, because 5/400 is under 1/100.
        let mut five = Starts::new();
        five.insert("a".to_string(), 5);
        let heavy_ahead = rank(0, 400, "a", 10, path, &five);
        assert!(light_level.before(&heavy_ahead));
    }

    #[test]
    fn before_me_counts_every_better_ticket() {
        let (dir, queue) = scratch("before");
        let first = queue.join("w", 0, "alice", 100);
        let second = queue.join("w", 0, "alice", 100);
        let third = queue.join("w", 0, "alice", 100);
        assert_eq!(queue.before_me(&first), 0);
        assert_eq!(queue.before_me(&second), 1);
        assert_eq!(queue.before_me(&third), 2);
        // Two of the three tickets rank ahead of the third one, so two free
        // slots let it through without waiting for the others to leave.
        assert_eq!(queue.before_me(&third), 2);
        drop(first);
        assert_eq!(queue.before_me(&third), 1);
        drop(second);
        assert!(queue.ahead(&third));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stale_ticket_is_ignored_and_removed() {
        let (dir, queue) = scratch("stale");
        let ghost = queue.join("w", -5, "ghost", 100);
        let now = FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(30));
        ghost.file.as_ref().expect("ticket").set_times(now).unwrap();
        let live = queue.join("w", 0, "alice", 100);
        assert!(
            queue.ahead(&live),
            "a dead waiter must not block a live one"
        );
        assert!(!ghost.path.exists(), "the stale ticket must be cleaned");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropping_the_lane_removes_its_ticket() {
        let (dir, queue) = scratch("drop");
        let lane = queue.join("w", 0, "alice", 100);
        let path = lane.path.clone();
        assert!(path.exists());
        drop(lane);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_broken_queue_dir_fails_open() {
        let (dir, _) = scratch("broken");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "not a dir").expect("write blocker");
        let queue = Queue::new(&blocker);
        let lane = queue.join("w", 0, "alice", 100);
        assert!(queue.ahead(&lane), "a broken queue must never stop a job");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn old_starts_are_forgotten() {
        let (dir, queue) = scratch("ttl");
        queue.charge("w", "alice");
        let served = queue.dir("w").join("served");
        let old = FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(600));
        File::options()
            .write(true)
            .open(&served)
            .unwrap()
            .set_times(old)
            .unwrap();
        let alice = queue.join("w", 0, "alice", 100);
        let bob = queue.join("w", 0, "bob", 100);
        // Alice's old lead is gone, so arrival order decides, not her history.
        assert!(queue.ahead(&alice));
        assert!(!queue.ahead(&bob));
        let _ = std::fs::remove_dir_all(dir);
    }
}
