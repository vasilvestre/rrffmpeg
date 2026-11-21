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
/// Holds the highest sequence number reserved, so processes never overlap.
const SEQ_NAME: &str = "journal.seq";
/// Numbers one process may use before it needs another block. A job writes a
/// start, a finish and the odd host or reap event, well inside this.
const SEQ_BLOCK: u64 = 1024;
/// Rotate once the active file passes this size. Old files stay as
/// `journal.1`, `journal.2`, and so on.
const ROTATE_BYTES: u64 = 8 * 1024 * 1024;
/// Longest a queued event waits before the flusher forces it to disk.
const SYNC_INTERVAL: Duration = Duration::from_millis(500);

/// The numbers read from the worker's own stderr, all optional: a job run with
/// `-loglevel error` or `-nostats` prints none of them.
///
/// `src_hdr` is empty for SDR and names the transfer function otherwise, so a
/// dashboard can count HDR jobs. Widths are pixels, `speed` is times realtime.
///
/// `fps` and `speed` are the highest running average the job reached, not its
/// last line: ffmpeg averages them since the start, so a slow start or a stall
/// drags the tail down below what the player showed live.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_w: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_h: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_hdr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_w: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_h: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_codec: Option<String>,
    /// `copy` or `encode` per output stream kind, read from ffmpeg's stream
    /// mapping. A video copy is a direct stream; an encode is a transcode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Started {
        job: String,
        host: String,
        /// Microseconds from process start to this append, on a monotonic
        /// clock. It is the dispatcher's own share of the start: config, the
        /// slot file. The transport comes after.
        ///
        /// Defaulted: journals written before this field existed must keep
        /// replaying, and their dispatch time was never measured.
        #[serde(default)]
        elapsed_us: u64,
        /// `ffmpeg` or `ffprobe`, so a dashboard can split the probes that
        /// flood Jellyfin's log from the transcodes that cost the time.
        /// Defaulted for journals written before it existed.
        #[serde(default)]
        program: String,
        /// Jobs already running on that host when this one started, this one
        /// included. Compare with the host's `concurrency` to read saturation.
        #[serde(default)]
        in_flight: usize,
        /// `light` or `heavy`, read from the program and the arguments (see
        /// `Class`). A Light job is a probe, a thumbnail or a trickplay scan.
        /// Defaulted for journals written before the split.
        #[serde(default)]
        class: String,
        /// The slot pool the job took: `heavy` or `light`. Equal to `class`
        /// today, and recorded apart so a dashboard reads where the job was
        /// placed instead of inferring it.
        #[serde(default)]
        pool: String,
        /// What the job does, read from the program and the arguments (see
        /// `Kind`): `trickplay`, `transcode`, and the rest. Defaulted for
        /// journals written before it existed.
        ///
        /// `job_kind` and not `kind`: the event's own tag already owns that
        /// name, and renaming the tag would stop old journals from replaying.
        #[serde(default)]
        job_kind: String,
        /// The input path with the `file:` scheme stripped, and empty when the
        /// job has no `-i`. One path, never the whole argument list.
        /// Defaulted for journals written before it existed.
        #[serde(default)]
        input: String,
    },
    Finished {
        job: String,
        code: i32,
        /// The worker that ran it, repeated here so a failed job is counted
        /// per host without joining two lines.
        #[serde(default)]
        host: String,
        /// The transport call, in microseconds: what the worker spent on the
        /// job. `rrffmpeg-stats` derives the same number from the timestamps,
        /// which Loki cannot join.
        #[serde(default)]
        dur_us: u64,
        /// What the worker's ffmpeg printed about the job: its speed, and the
        /// shape of the source and the output. Flattened so one line holds
        /// `ev.fps`, `ev.src_w` and the rest, and empty when the job printed
        /// nothing to read. Boxed so the common `Started` variant stays small.
        #[serde(flatten)]
        stats: Box<Stats>,
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
    /// The heavy cap a process resolved at startup, with the numbers behind
    /// it, so a change has a reason in the journal. An audit line, not state:
    /// the next process reads the same tail and computes the same cap, and
    /// nothing reads this event back (ADR 0033). Defaulted fields keep a
    /// partial line replayable.
    Sized {
        host: String,
        /// The content class the cap is for, see `sizing::Key`.
        #[serde(default)]
        class: String,
        #[serde(default)]
        cap: usize,
        /// Finishes the percentile was read from.
        #[serde(default)]
        samples: usize,
        /// 5th percentile of the solo speeds, in times realtime.
        #[serde(default)]
        p05: Option<f64>,
        /// The cap before the hysteresis move, so a change has a reason.
        #[serde(default)]
        previous: usize,
    },
    /// A job that stopped using the CPU and stopped writing: Jellyfin paused
    /// the stream, so the job is waiting on a viewer, not encoding. It keeps
    /// its slot, and placement discounts it.
    Parked {
        job: String,
    },
    /// The same job took the CPU again, or wrote again, without a new start.
    /// It needs no new slot.
    Resumed {
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
    /// Last sequence number assigned by this process.
    seq: u64,
    /// First number of the next block, exclusive of the current one.
    block_end: u64,
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
        // Reserve once per process: a per-append lock would put a cross-process
        // syscall on the dispatch path.
        let base = reserve_seq(dir, &path, &rotated)?;
        let live = Live {
            seq: base,
            block_end: base + SEQ_BLOCK,
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
        // Blocks are exclusive per process, so file order is not seq order.
        // A set keeps the first copy of a 0.8.0-era duplicate without dropping
        // records from a lower block that were written later.
        let mut seen = HashSet::new();
        for (_, path) in &rotated {
            if replay(path, &mut live, &mut seen).is_err() {
                return;
            }
        }
        if replay(&self.dir.join(JOURNAL_NAME), &mut live, &mut seen).is_err() {
            return;
        }
        live.replayed = true;
    }

    /// Queue an event. Touches only memory and an unbounded channel: no file
    /// IO, no fsync, and no panic when the flusher has gone away.
    pub fn append(&self, event: Event) {
        let record = {
            let mut live = lock(&self.live);
            // A block lasts 1024 appends; a long-lived process takes the next
            // one here, so it cannot run into another process's block.
            if live.seq >= live.block_end {
                let rotated = rotated_files(&self.dir).unwrap_or_default();
                let active = self.dir.join(JOURNAL_NAME);
                if let Ok(base) = reserve_seq(&self.dir, &active, &rotated) {
                    live.seq = live.seq.max(base);
                    live.block_end = live.seq + SEQ_BLOCK;
                }
            }
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

    pub fn active_jobs(&self) -> Vec<(String, String)> {
        self.ensure_replayed();
        lock(&self.live)
            .jobs
            .iter()
            .map(|(job, host)| (job.clone(), host.clone()))
            .collect()
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
        // The host verdict lives in the lock directory. These events are for
        // `rrffmpeg-stats` only.
        Event::HostDown { .. } | Event::HostUp { .. } => {}
        // A parked job is still running and keeps its active row: it owns a
        // slot, and it will print into the same Finished line when it ends.
        // The cap line is an audit record of one process's startup.
        Event::Parked { .. } | Event::Resumed { .. } | Event::Sized { .. } => {}
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

/// Every record on disk, oldest first, one entry per `seq`. Since ADR 0025
/// sequence numbers are unique, so the map only collapses a journal written by
/// 0.8.0 or earlier, where several processes could take the same number.
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

/// Take the next block of sequence numbers under an exclusive lock on the
/// reservation file. Returns the last number in use, so the first append takes
/// the next one. Every failure falls back to the journal tail: a broken state
/// directory must not stop a transcode.
fn reserve_seq(dir: &Path, active: &Path, rotated: &[(u64, PathBuf)]) -> Result<u64> {
    let disk = std::iter::once(active)
        .chain(rotated.iter().map(|(_, path)| path.as_path()))
        .map(tail_seq)
        .max()
        .unwrap_or(0);
    let path = dir.join(SEQ_NAME);
    let mut file = match OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!(
                "rrffmpeg: journal {}: {e}, sequence numbers may collide",
                path.display()
            );
            return Ok(disk);
        }
    };
    if let Err(e) = file.lock() {
        eprintln!(
            "rrffmpeg: journal {}: {e}, sequence numbers may collide",
            path.display()
        );
        return Ok(disk);
    }

    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    let base = text.trim().parse::<u64>().unwrap_or(0).max(disk);
    // No fsync: another process on this host reads the page cache, and a crash
    // loses only the unused tail of the block.
    let next = base + SEQ_BLOCK;
    let write = (|| -> io::Result<()> {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(next.to_string().as_bytes())
    })();
    if let Err(e) = write {
        eprintln!(
            "rrffmpeg: journal {}: {e}, sequence numbers may collide",
            path.display()
        );
    }
    Ok(base)
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

/// The last records of the active journal, oldest first, at most the last
/// 64 KB of the file.
///
/// The sizing reads this instead of the whole journal: one startup pays for
/// 64 KB, not for a week of history. A missing file is no records, which
/// sizes to one job. A seek that lands inside a line leaves a fragment that
/// does not parse, and it is dropped like any other bad line.
pub fn tail_records(dir: &Path) -> Vec<Record> {
    const TAIL: u64 = 64 * 1024;
    let path = dir.join(JOURNAL_NAME);
    let Ok(mut file) = File::open(&path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(TAIL);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if (&mut file).take(TAIL).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    lines
        .filter_map(|line| serde_json::from_str::<Record>(line).ok())
        .collect()
}

/// Rebuild live state from one file. A bad line is reported and skipped: a
/// truncated tail after a crash is expected, not fatal.
fn replay(path: &Path, live: &mut Live, seen: &mut HashSet<u64>) -> Result<()> {
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
                if !seen.insert(record.seq) {
                    continue;
                }
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
        let mut stop = false;

        match rx.recv_timeout(writer.sync_interval) {
            Ok(Command::Record(record)) => {
                batch.push(record);
                // Take everything already queued so one syscall covers them.
                loop {
                    match rx.try_recv() {
                        Ok(Command::Record(record)) => batch.push(record),
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
            Ok(Command::Stop) => stop = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => stop = true,
        }

        writer.commit(&mut batch);
        if stop || writer.sync_due() {
            writer.sync();
        }
        if stop {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

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
            program: "ffmpeg".into(),
            in_flight: 1,
            class: "heavy".into(),
            pool: "heavy".into(),
            job_kind: "transcode".into(),
            input: "/media/ep.mkv".into(),
        }
    }

    /// Sequence numbers as written, not as `records()` dedups them: a duplicate
    /// is the defect these tests must see.
    fn raw_seqs(path: &Path) -> Vec<u64> {
        let file = File::open(path).unwrap();
        BufReader::new(file)
            .lines()
            .map(|line| serde_json::from_str::<Record>(&line.unwrap()).unwrap().seq)
            .collect()
    }

    fn records(path: &Path) -> Vec<Record> {
        let file = File::open(path).unwrap();
        BufReader::new(file)
            .lines()
            .map(|line| serde_json::from_str::<Record>(&line.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn the_tail_reader_takes_what_parses_and_nothing_without_a_journal() {
        let dir = temp_dir("tail-records");
        assert!(tail_records(&dir).is_empty(), "no journal, no samples");

        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(started("a", "worker"));
            journal.append(started("b", "worker"));
        }
        let records = tail_records(&dir);
        assert_eq!(records.len(), 2);
        assert!(matches!(&records[1].ev, Event::Started { job, .. } if job == "b"));

        // A crash can leave half a line at the end of the file; the reader
        // drops the fragment and keeps everything it can parse.
        let path = dir.join(JOURNAL_NAME);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"seq\":9,\"ts\":9,\"ev\":{\"kind\":\"star");
        std::fs::write(&path, text).unwrap();
        assert_eq!(tail_records(&dir).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_of_a_directory_with_no_journal_is_empty() {
        let dir = temp_dir("records-none");
        assert!(super::records(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_skip_blank_and_unparsable_lines() {
        let dir = temp_dir("records-skip");
        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(started("a", "worker"));
        }
        let path = dir.join(JOURNAL_NAME);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\nnot json\n\n");
        std::fs::write(&path, text).unwrap();
        assert_eq!(super::records(&dir).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_has_no_tail_sequence() {
        let dir = temp_dir("tail-missing");
        assert_eq!(tail_seq(&dir.join("nope")), 0);
        assert!(rotated_files(&dir.join("nope")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counters_are_correct_before_any_sync() {
        let dir = temp_dir("counters");
        let journal = Journal::open(&dir).unwrap();
        journal.append(started("a", "worker"));
        journal.append(started("b", "worker"));
        journal.append(started("c", "client"));
        journal.append(Event::Finished {
            job: "a".into(),
            code: 0,
            host: "worker".into(),
            dur_us: 0,
            stats: Box::default(),
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
        // Nothing reached the disk yet, and the counters are already right.
    }

    #[test]
    fn reopen_replays_and_counters_match() {
        let dir = temp_dir("reopen");
        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(started("a", "worker"));
            journal.append(started("b", "worker"));
        }
        // Dropping the handle flushes it, so the reopen sees every record.
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 2);
        assert_eq!(journal.active_jobs().len(), 2);
    }

    #[test]
    fn killed_job_stays_active_until_reaped() {
        let dir = temp_dir("killed");
        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(started("orphan", "worker"));
        }
        // SIGKILL skipped the Finished event: the row survives the restart.
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 1);

        journal.append(Event::Reaped {
            job: "orphan".into(),
        });
        assert_eq!(journal.active_on("worker"), 0);

        drop(journal);
        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 0);
        assert!(journal.active_jobs().is_empty());
    }

    /// `rrffmpeg-stats` counts host events, so they must survive the round
    /// trip through the journal even though the dispatcher keeps its own
    /// verdict in the lock directory.
    #[test]
    fn host_events_survive_replay() {
        let dir = temp_dir("hosts");
        {
            let journal = Journal::open(&dir).unwrap();
            journal.append(Event::HostDown {
                host: "worker".into(),
            });
            journal.append(Event::HostUp {
                host: "worker".into(),
            });
        }
        let events: Vec<Event> = records(&dir.join(JOURNAL_NAME))
            .into_iter()
            .map(|r| r.ev)
            .collect();
        assert!(matches!(events[0], Event::HostDown { .. }));
        assert!(matches!(events[1], Event::HostUp { .. }));
    }

    #[test]
    fn rotation_writes_a_second_file_and_keeps_state() {
        let dir = temp_dir("rotate");
        let journal = Journal::open_inner(&dir, SYNC_INTERVAL, 300).unwrap();
        for i in 0..20 {
            journal.append(started(&format!("job{i}"), "worker"));
        }
        assert_eq!(journal.active_on("worker"), 20);
        assert_eq!(journal.active_jobs().len(), 20);

        // Rotated content and state both survive a restart.
        drop(journal);
        assert!(dir.join("journal.1").exists(), "expected a rotated file");
        assert!(dir.join("journal").exists(), "expected a fresh active file");
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
        assert_eq!(journal.active_on("worker"), 2);
    }

    /// A journal written before `elapsed_us`, `kind` and `input` existed keeps
    /// replaying, without a warning per line at every start.
    #[test]
    fn a_record_without_elapsed_us_still_counts() {
        let dir = temp_dir("legacy");
        let legacy = "{\"seq\":1,\"ts\":0,\"ev\":{\"kind\":\"started\",\"job\":\"a\",\"host\":\"worker\"}}\n";
        std::fs::write(dir.join(JOURNAL_NAME), legacy).unwrap();

        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 1);

        // The missing fields default to empty, not to a parse failure.
        let record = &super::records(&dir).unwrap()[0];
        match &record.ev {
            Event::Started {
                job_kind, input, ..
            } => {
                assert!(job_kind.is_empty());
                assert!(input.is_empty());
            }
            other => panic!("expected a Started event, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
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

        assert_eq!(journal.active_on("worker"), 4000);
        assert_eq!(journal.active_jobs().len(), 4000);
        drop(journal);

        let lines = records(&dir.join(JOURNAL_NAME));
        assert_eq!(lines.len(), 4000);
        let mut seqs: Vec<u64> = lines.iter().map(|r| r.seq).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 4000, "sequence numbers must be unique");
    }

    /// Jellyfin starts one process per job, so each handle opens the same
    /// directory and must take numbers no other process will use.
    #[test]
    fn separate_handles_never_share_a_sequence_number() {
        let dir = temp_dir("cross-process-seq");
        const WRITERS: usize = 8;
        const PER_WRITER: usize = 50;
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut handles = Vec::new();
        for t in 0..WRITERS {
            let dir = dir.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let journal = Journal::open(&dir).unwrap();
                for i in 0..PER_WRITER {
                    journal.append(started(&format!("job-{t}-{i}"), "worker"));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let lines = records(&dir.join(JOURNAL_NAME));
        assert_eq!(lines.len(), WRITERS * PER_WRITER);
        let mut seqs = raw_seqs(&dir.join(JOURNAL_NAME));
        assert_eq!(seqs.len(), WRITERS * PER_WRITER);
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            seqs.len(),
            WRITERS * PER_WRITER,
            "two processes took the same sequence number"
        );
    }

    /// Reading the journal for the janitor must not move this process past its
    /// reserved block. Two handles that both replay used to append the same
    /// number, which is how 0.8.1 still lost records.
    #[test]
    fn replay_does_not_move_the_writer_past_its_block() {
        let dir = temp_dir("replay-block");
        {
            let first = Journal::open(&dir).unwrap();
            let second = Journal::open(&dir).unwrap();
            let third = Journal::open(&dir).unwrap();

            // Write past the second and third blocks, so their replay would
            // land on the same high number without a per-process block.
            for i in 0..SEQ_BLOCK * 2 + 2 {
                first.append(started(&format!("a-{i}"), "worker"));
            }

            // Both replay the same high tail, as the janitor does in every
            // process.
            let _ = second.active_on("worker");
            let _ = third.active_on("worker");
            second.append(started("b", "worker"));
            third.append(started("c", "worker"));
        }

        let lines = records(&dir.join(JOURNAL_NAME));
        let total = lines.len();
        let mut seqs = raw_seqs(&dir.join(JOURNAL_NAME));
        assert_eq!(seqs.len(), total);
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            seqs.len(),
            total,
            "replay moved a writer into another block"
        );
    }

    /// A process that outlives its first 1024 numbers takes a new block instead
    /// of running into the next process's.
    #[test]
    fn a_long_lived_process_takes_a_new_block() {
        let dir = temp_dir("block-renewal");
        let journal = Journal::open(&dir).unwrap();
        let n = (SEQ_BLOCK * 2 + 10) as usize;
        for i in 0..n {
            journal.append(started(&format!("j{i}"), "worker"));
        }
        drop(journal);

        let lines = records(&dir.join(JOURNAL_NAME));
        assert_eq!(lines.len(), n);
        let mut seqs = raw_seqs(&dir.join(JOURNAL_NAME));
        assert_eq!(seqs.len(), n);
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), n, "a block overflow was not renewed");
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

    /// A crash can leave a rotated segment with a half-written first line.
    /// Replay skips that line and still applies the records after it, in that
    /// segment and in the active file.
    #[test]
    fn a_rotated_file_with_a_truncated_header_does_not_stop_replay() {
        let dir = temp_dir("rotated-truncated");
        let truncated = "{\"seq\":1,\"ts\":0,\"ev\":{\"kind\":\"started\",\"jo";
        let first = serde_json::to_string(&Record {
            seq: 2,
            ts: 0,
            ev: started("a", "worker"),
        })
        .unwrap();
        std::fs::write(dir.join("journal.1"), format!("{truncated}\n{first}\n")).unwrap();

        let second = serde_json::to_string(&Record {
            seq: 3,
            ts: 0,
            ev: started("b", "worker"),
        })
        .unwrap();
        std::fs::write(dir.join(JOURNAL_NAME), format!("{second}\n")).unwrap();

        let journal = Journal::open(&dir).unwrap();
        assert_eq!(journal.active_on("worker"), 2);
        let mut jobs = journal.active_jobs();
        jobs.sort();
        assert_eq!(
            jobs,
            vec![
                ("a".to_string(), "worker".to_string()),
                ("b".to_string(), "worker".to_string()),
            ]
        );
    }

    /// The janitor may see a job id it never recorded, for example after a
    /// state dir swap. Reaping it must not touch the jobs that are running.
    #[test]
    fn reap_of_an_unknown_job_is_a_no_op() {
        let dir = temp_dir("reap-unknown");
        let journal = Journal::open(&dir).unwrap();
        journal.append(started("a", "worker"));

        journal.append(Event::Reaped {
            job: "ghost".into(),
        });

        assert_eq!(journal.active_on("worker"), 1);
        assert_eq!(
            journal.active_jobs(),
            vec![("a".to_string(), "worker".to_string())]
        );
    }

    /// `records` merges the active and the rotated files, so the caller sees
    /// one sequence, not file order.
    #[test]
    fn records_orders_rotated_and_active_files_by_sequence() {
        let dir = temp_dir("records-order");
        let record = |seq: u64, job: &str| {
            serde_json::to_string(&Record {
                seq,
                ts: 0,
                ev: started(job, "worker"),
            })
            .unwrap()
        };
        // Written out of sequence order and out of file order: the sort must
        // come from `seq`, not from the read order.
        std::fs::write(dir.join("journal.2"), format!("{}\n", record(30, "c"))).unwrap();
        std::fs::write(dir.join("journal.1"), format!("{}\n", record(10, "a"))).unwrap();
        std::fs::write(dir.join(JOURNAL_NAME), format!("{}\n", record(20, "b"))).unwrap();

        let seqs: Vec<u64> = super::records(&dir)
            .unwrap()
            .iter()
            .map(|r| r.seq)
            .collect();
        assert_eq!(seqs, vec![10, 20, 30]);
    }
}
