//! Append-only job journal.
//!
//! The write is queued and flushed by a background thread, so a job never
//! waits for an fsync. Counters live in memory and are updated before the
//! queue send, so async writes cannot undercount running jobs.
//!
//! On disk each event is one JSON object per line:
//!
//! ```text
//! {"seq":1,"ts":1763300000123456,"ev":{"kind":"started","job":"42-7","host":"worker","elapsed_us":412}}
//! ```
//!
//! `seq` orders the events and `ts` is wall-clock microseconds, because the
//! dispatcher has to be measured at a finer grain than a millisecond. A truncated
//! last line fails to parse and is skipped, so a crash mid-write costs at most
//! the event being written.

use crate::Error;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const JOURNAL_NAME: &str = "journal";
/// Rotate once the active file passes this size. Old files stay as
/// `journal.1`, `journal.2`, and so on.
const ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Longest a queued event waits before the flusher forces it to disk.
const SYNC_INTERVAL: Duration = Duration::from_millis(500);
/// Bound on the wait for a flush ack, so a wedged flusher cannot hang forever.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Started {
        job: String,
        host: String,
        /// Microseconds from process start to this append, on a monotonic
        /// clock. It is the dispatcher's own share of the start: config, the
        /// journal replay, the slot file. The transport comes after.
        ///
        /// Defaulted: journals written before this field existed must keep
        /// replaying, and their dispatch time was never measured.
        #[serde(default)]
        elapsed_us: u64,
    },
    Finished {
        job: String,
        code: i32,
    },
    HostDown {
        host: String,
    },
    HostUp {
        host: String,
    },
    /// A job whose process disappeared without a Finished event.
    Reaped {
        job: String,
    },
}

/// One line of the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    pub ts: u64,
    pub ev: Event,
}

enum Command {
    Record(Record),
    Flush(Sender<()>),
    Stop,
}

/// Live state, current for this process, and rebuilt from the journal on the
/// first read that needs it.
#[derive(Default)]
struct Live {
    /// job id to host. A Started with no Finished stays here: that is the
    /// orphan the janitor repairs.
    jobs: HashMap<String, String>,
    /// Running count per host. Maintained alongside `jobs` so selection never
    /// scans the map.
    counts: HashMap<String, usize>,
    down: HashSet<String>,
    /// Last sequence number assigned or replayed.
    seq: u64,
    /// Highest sequence number replayed, used to ignore duplicates.
    last_seq: u64,
    /// The journal has been replayed into this state. Only `append` runs before
    /// the janitor asks for the active jobs, and that read is what replays.
    replayed: bool,
}

pub struct Journal {
    dir: PathBuf,
    tx: Sender<Command>,
    live: Mutex<Live>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Journal {
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub fn open(dir: &Path) -> Result<Journal> {
        Self::open_inner(dir, SYNC_INTERVAL, ROTATE_BYTES)
    }

    fn open_inner(dir: &Path, sync_interval: Duration, rotate_bytes: u64) -> Result<Journal> {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Io(e, format!("create {}", dir.display())))?;

        let mut rotated = rotated_files(dir)?;
        rotated.sort_by_key(|(n, _)| *n);
        let path = dir.join(JOURNAL_NAME);

        // Only the tail is read here. Replaying the whole file costs about
        // 4 us/KB, so at the 8 MB rotation cap it would spend 30 ms of the
        // dispatch budget on state the janitor needs and the dispatch does not.
        let live = Live {
            seq: last_seq_on_disk(&path, &rotated),
            ..Live::default()
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::Io(e, format!("open {}", path.display())))?;
        let len = file
            .metadata()
            .map_err(|e| Error::Io(e, format!("stat {}", path.display())))?
            .len();

        let writer = Writer {
            dir: dir.to_path_buf(),
            path: path.clone(),
            file: Some(file),
            len,
            next_rotated: rotated.last().map(|(n, _)| n + 1).unwrap_or(1),
            rotate_bytes,
            sync_interval,
            last_sync: Instant::now(),
            dirty: false,
        };

        let (tx, rx) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("rrffmpeg-journal".into())
            .spawn(move || flusher_loop(writer, rx))
            .map_err(|e| Error::Io(e, "spawn journal flusher".into()))?;

        Ok(Journal {
            dir: dir.to_path_buf(),
            tx,
            live: Mutex::new(live),
            handle: Mutex::new(Some(handle)),
        })
    }

    /// Replay the journal into the in-memory state, once. The janitor is the
    /// only reader, and it runs off the dispatch path, so the cost lands there.
    fn ensure_replayed(&self) {
        let mut live = lock(&self.live);
        if live.replayed {
            return;
        }
        let Ok(mut rotated) = rotated_files(&self.dir) else {
            return;
        };
        rotated.sort_by_key(|(n, _)| *n);
        for (_, path) in &rotated {
            if replay(path, &mut live).is_err() {
                return;
            }
        }
        if replay(&self.dir.join(JOURNAL_NAME), &mut live).is_err() {
            return;
        }
        live.replayed = true;
    }

    /// Queue an event. Touches only memory and an unbounded channel: no file
    /// IO, no fsync, and no panic when the flusher has gone away.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    pub fn append(&self, event: Event) {
        let record = {
            let mut live = lock(&self.live);
            live.seq += 1;
            apply(&event, &mut live);
            Record {
                seq: live.seq,
                ts: now_micros(),
                ev: event,
            }
        };
        // A dropped receiver means the flusher died. The in-memory state is
        // already correct, so the job still dispatches; durability is lost.
        let _ = self.tx.send(Command::Record(record));
    }

    pub fn active_on(&self, host: &str) -> usize {
        self.ensure_replayed();
        lock(&self.live).counts.get(host).copied().unwrap_or(0)
    }

    pub fn is_down(&self, host: &str) -> bool {
        self.ensure_replayed();
        lock(&self.live).down.contains(host)
    }

    pub fn active_jobs(&self) -> Vec<(String, String)> {
        self.ensure_replayed();
        lock(&self.live)
            .jobs
            .iter()
            .map(|(job, host)| (job.clone(), host.clone()))
            .collect()
    }

    /// Block until everything queued so far is written and synced. Never call
    /// this on the dispatch path.
    pub fn flush(&self) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.tx
            .send(Command::Flush(ack_tx))
            .map_err(|_| Error::State("journal flusher is gone".into()))?;
        ack_rx
            .recv_timeout(FLUSH_TIMEOUT)
            .map_err(|_| Error::State("journal flush timed out".into()))
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        let handle = lock(&self.handle).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn apply(event: &Event, live: &mut Live) {
    match event {
        Event::Started { job, host, .. } => {
            if let Some(old) = live.jobs.insert(job.clone(), host.clone()) {
                dec(&mut live.counts, &old);
            }
            *live.counts.entry(host.clone()).or_insert(0) += 1;
        }
        Event::Finished { job, .. } | Event::Reaped { job } => {
            if let Some(host) = live.jobs.remove(job) {
                dec(&mut live.counts, &host);
            }
        }
        Event::HostDown { host } => {
            live.down.insert(host.clone());
        }
        Event::HostUp { host } => {
            live.down.remove(host);
        }
    }
}

fn dec(counts: &mut HashMap<String, usize>, host: &str) {
    if let Some(n) = counts.get_mut(host) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            counts.remove(host);
        }
    }
}

/// Every record on disk, oldest first, one entry per `seq`. Rotation writes
/// `journal.N` beside the active `journal`, and a crash can repeat a sequence
/// number, so the map wins twice.
pub fn records(dir: &Path) -> Result<Vec<Record>> {
    let mut files = vec![dir.join(JOURNAL_NAME)];
    files.extend(rotated_files(dir)?.into_iter().map(|(_, path)| path));
    let mut by_seq: std::collections::BTreeMap<u64, Record> = std::collections::BTreeMap::new();
    for path in files {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::Io(e, format!("open {}", path.display()))),
        };
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|e| Error::Io(e, format!("read {}", path.display())))?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(record) = serde_json::from_str::<Record>(&line) {
                by_seq.insert(record.seq, record);
            }
        }
    }
    Ok(by_seq.into_values().collect())
}

fn rotated_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e, format!("read {}", dir.display()))),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::Io(e, format!("read {}", dir.display())))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix("journal.") else {
            continue;
        };
        let Ok(n) = rest.parse::<u64>() else { continue };
        out.push((n, entry.path()));
    }
    Ok(out)
}

/// Rebuild live state from one file. A bad line is reported and skipped: a
/// truncated tail after a crash is expected, not fatal.
/// Highest `seq` already on disk, from the tail of the active file, or of the
/// newest rotated one when the active file holds no record yet.
fn last_seq_on_disk(active: &Path, rotated: &[(u64, PathBuf)]) -> u64 {
    let seq = tail_seq(active);
    if seq > 0 {
        return seq;
    }
    rotated
        .iter()
        .rev()
        .map(|(_, path)| tail_seq(path))
        .find(|seq| *seq > 0)
        .unwrap_or(0)
}

/// The last complete record of one file, so a restart resumes the sequence
/// without reading the whole journal. A truncated final line is skipped.
fn tail_seq(path: &Path) -> u64 {
    const TAIL: u64 = 8192;
    let Ok(mut file) = File::open(path) else {
        return 0;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return 0;
    };
    if file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL)))
        .is_err()
    {
        return 0;
    }
    let mut buf = Vec::new();
    if file.take(TAIL).read_to_end(&mut buf).is_err() {
        return 0;
    }
    String::from_utf8_lossy(&buf)
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Record>(line).ok())
        .map(|record| record.seq)
        .unwrap_or(0)
}

fn replay(path: &Path, live: &mut Live) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e, format!("open {}", path.display()))),
    };
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let line = match line {
            Ok(line) if !line.trim().is_empty() => line,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("rrffmpeg: journal {}: unreadable line: {e}", path.display());
                break;
            }
        };
        match serde_json::from_str::<Record>(&line) {
            Ok(record) => {
                if record.seq <= live.last_seq {
                    continue;
                }
                live.last_seq = record.seq;
                live.seq = live.seq.max(record.seq);
                apply(&record.ev, live);
            }
            Err(e) => eprintln!(
                "rrffmpeg: journal {}: skipping line {}: {e}",
                path.display(),
                idx + 1
            ),
        }
    }
    Ok(())
}

struct Writer {
    dir: PathBuf,
    path: PathBuf,
    file: Option<File>,
    len: u64,
    next_rotated: u64,
    rotate_bytes: u64,
    sync_interval: Duration,
    last_sync: Instant,
    /// Bytes written since the last sync.
    dirty: bool,
}

impl Writer {
    fn commit(&mut self, batch: &mut Vec<Record>) {
        if batch.is_empty() {
            return;
        }
        match self.write_batch(batch) {
            Ok(()) => {
                self.dirty = true;
                batch.clear();
            }
            Err(e) => {
                // Drop the batch: blocking on a broken disk is worse than
                // losing an advisory record.
                eprintln!("rrffmpeg: journal write failed: {e}");
                batch.clear();
            }
        }
        if let Err(e) = self.rotate_if_needed() {
            eprintln!("rrffmpeg: journal rotation failed: {e}");
        }
    }

    fn write_batch(&mut self, batch: &[Record]) -> Result<()> {
        let mut bytes = Vec::new();
        for record in batch {
            serde_json::to_writer(&mut bytes, record)
                .map_err(|e| Error::State(format!("encode journal record: {e}")))?;
            bytes.push(b'\n');
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::State("journal file is closed".into()))?;
        file.write_all(&bytes)
            .map_err(|e| Error::Io(e, format!("write {}", self.path.display())))?;
        file.flush()
            .map_err(|e| Error::Io(e, format!("flush {}", self.path.display())))?;
        self.len += bytes.len() as u64;
        Ok(())
    }

    fn rotate_if_needed(&mut self) -> Result<()> {
        if self.len <= self.rotate_bytes {
            return Ok(());
        }
        self.file = None;
        let rotated = self
            .dir
            .join(format!("{JOURNAL_NAME}.{}", self.next_rotated));
        std::fs::rename(&self.path, &rotated)
            .map_err(|e| Error::Io(e, format!("rename to {}", rotated.display())))?;
        self.next_rotated += 1;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| Error::Io(e, format!("open {}", self.path.display())))?;
        self.file = Some(file);
        self.len = 0;
        Ok(())
    }

    fn sync_due(&self) -> bool {
        self.dirty && self.last_sync.elapsed() >= self.sync_interval
    }

    fn sync(&mut self) {
        if !self.dirty {
            return;
        }
        if let Some(file) = self.file.as_ref()
            && let Err(e) = file.sync_data()
        {
            eprintln!("rrffmpeg: journal sync failed: {e}");
        }
        self.last_sync = Instant::now();
        self.dirty = false;
    }
}

fn flusher_loop(mut writer: Writer, rx: Receiver<Command>) {
    let mut batch: Vec<Record> = Vec::new();
    loop {
        let mut acks: Vec<Sender<()>> = Vec::new();
        let mut sync_now = false;
        let mut stop = false;

        match rx.recv_timeout(writer.sync_interval) {
            Ok(Command::Record(record)) => {
                batch.push(record);
                // Take everything already queued so one syscall covers them.
                loop {
                    match rx.try_recv() {
                        Ok(Command::Record(record)) => batch.push(record),
                        Ok(Command::Flush(ack)) => {
                            acks.push(ack);
                            sync_now = true;
                        }
                        Ok(Command::Stop) => {
                            stop = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            stop = true;
                            break;
                        }
                    }
                }
            }
            Ok(Command::Flush(ack)) => {
                acks.push(ack);
                sync_now = true;
            }
            Ok(Command::Stop) => stop = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => stop = true,
        }

        writer.commit(&mut batch);
        if stop || sync_now || writer.sync_due() {
            writer.sync();
            for ack in acks.drain(..) {
                let _ = ack.send(());
            }
        }
        if stop {
            // Senders may still be waiting if Stop raced a Flush in the
            // batch drain loop.
            for ack in acks.drain(..) {
                let _ = ack.send(());
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rrffmpeg-state-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn started(job: &str, host: &str) -> Event {
        Event::Started {
            job: job.into(),
            host: host.into(),
            elapsed_us: 0,
        }
    }

    fn records(path: &Path) -> Vec<Record> {
        let file = File::open(path).unwrap();
        BufReader::new(file)
            .lines()
            .map(|line| serde_json::from_str::<Record>(&line.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn counters_are_correct_before_any_flush() {
        let dir = temp_dir("counters");
        let journal = Journal::open(&dir).unwrap();
        journal.append(started("a", "worker"));
        journal.append(started("b", "worker"));
        journal.append(started("c", "client"));
        journal.append(Event::Finished {
            job: "a".into(),
            code: 0,
        });

        assert_eq!(journal.active_on("worker"), 1);
        assert_eq!(journal.active_on("client"), 1);
        let mut jobs = journal.active_jobs();
        jobs.sort();
        assert_eq!(
            jobs,
            vec![
                ("b".to_string(), "worker".to_string()),
                ("c".to_string(), "client".to_string()),
            ]
        );
        // Nothing forced the flusher, yet the counters are already right.
        assert!(!journal.is_down("worker"));
    }

    #[test]
    fn reopen_replays_and_counters_match() {
        let dir = temp_dir("reopen");
        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(started("a", "worker"));
            journal.append(started("b", "worker"));
            journal.append(Event::HostDown {
                host: "worker".into(),
            });
            journal.flush().unwrap();
        }
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 2);
        assert!(journal.is_down("worker"));
        assert_eq!(journal.active_jobs().len(), 2);
    }

    #[test]
    fn killed_job_stays_active_until_reaped() {
        let dir = temp_dir("killed");
        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(started("orphan", "worker"));
            journal.flush().unwrap();
        }
        // SIGKILL skipped the Finished event: the row survives the restart.
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 1);

        journal.append(Event::Reaped {
            job: "orphan".into(),
        });
        journal.flush().unwrap();
        assert_eq!(journal.active_on("worker"), 0);

        drop(journal);
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 0);
        assert!(journal.active_jobs().is_empty());
    }

    #[test]
    fn host_down_and_up_round_trip() {
        let dir = temp_dir("hosts");
        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(Event::HostDown {
                host: "worker".into(),
            });
            journal.flush().unwrap();
        }
        let journal = Journal::open(&dir).unwrap();
        assert!(journal.is_down("worker"));

        journal.append(Event::HostUp {
            host: "worker".into(),
        });
        journal.flush().unwrap();
        assert!(!journal.is_down("worker"));

        drop(journal);
        let journal = Journal::open(&dir).unwrap();
        assert!(!journal.is_down("worker"));
    }

    #[test]
    fn rotation_writes_a_second_file_and_keeps_state() {
        let dir = temp_dir("rotate");
        let journal = Journal::open_inner(&dir, SYNC_INTERVAL, 300).unwrap();
        for i in 0..20 {
            journal.append(started(&format!("job{i}"), "worker"));
        }
        journal.flush().unwrap();

        assert!(dir.join("journal.1").exists(), "expected a rotated file");
        assert!(dir.join("journal").exists(), "expected a fresh active file");
        assert_eq!(journal.active_on("worker"), 20);
        assert_eq!(journal.active_jobs().len(), 20);

        // Rotated content and state both survive a restart.
        drop(journal);
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 20);
    }

    #[test]
    fn truncated_final_line_is_skipped_without_panic() {
        let dir = temp_dir("truncated");
        let good = serde_json::to_string(&Record {
            seq: 1,
            ts: 0,
            ev: started("a", "worker"),
        })
        .unwrap();
        let mut body = good;
        body.push('\n');
        body.push_str("{\"seq\":2,\"ts\":0,\"ev\":{\"kind\":\"started\",\"job\":\"b\",");
        std::fs::write(dir.join(JOURNAL_NAME), body).unwrap();

        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 1);
        assert_eq!(
            journal.active_jobs(),
            vec![("a".to_string(), "worker".to_string())]
        );

        // Unknown event kinds are skipped the same way.
        journal.append(started("c", "worker"));
        journal.flush().unwrap();
        assert_eq!(journal.active_on("worker"), 2);
    }

    /// A journal written before `elapsed_us` existed keeps replaying, without
    /// a warning per line at every start.
    #[test]
    fn a_record_without_elapsed_us_still_counts() {
        let dir = temp_dir("legacy");
        let legacy = "{\"seq\":1,\"ts\":0,\"ev\":{\"kind\":\"started\",\"job\":\"a\",\"host\":\"worker\"}}\n";
        std::fs::write(dir.join(JOURNAL_NAME), legacy).unwrap();

        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 1);
    }

    #[test]
    fn concurrent_appends_all_reach_disk() {
        let dir = temp_dir("concurrent");
        let journal = Arc::new(Journal::open(&dir).unwrap());
        let mut handles = Vec::new();
        for t in 0..8 {
            let journal = journal.clone();
            handles.push(thread::spawn(move || {
                for i in 0..500 {
                    journal.append(started(&format!("job-{t}-{i}"), "worker"));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        journal.flush().unwrap();

        assert_eq!(journal.active_on("worker"), 4000);
        assert_eq!(journal.active_jobs().len(), 4000);

        let lines = records(&dir.join(JOURNAL_NAME));
        assert_eq!(lines.len(), 4000);
        let mut seqs: Vec<u64> = lines.iter().map(|r| r.seq).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 4000, "sequence numbers must be unique");
    }

    #[test]
    fn append_latency_is_far_below_the_old_fsync() {
        let dir = temp_dir("latency");
        let journal = Journal::open(&dir).unwrap();
        const N: usize = 2000;
        let start = Instant::now();
        for i in 0..N {
            journal.append(started(&format!("job{i}"), "worker"));
        }
        let average = start.elapsed() / N as u32;
        println!("append average: {average:?} over {N} calls");
        assert!(
            average < Duration::from_millis(1),
            "append average {average:?} is too slow"
        );
        assert_eq!(journal.active_on("worker"), N);
    }
}
