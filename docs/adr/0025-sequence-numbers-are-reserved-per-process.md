# 0025: Sequence numbers are reserved per process, under a lock at open

Status: Accepted
Date: 2026-11-18
Note: replay still moved the writer, so 0.8.1 did not fully deliver this.
[ADR 0026](0026-a-block-is-exclusive-and-replay-never-moves-the-writer.md)
completes it.

## Context

Jellyfin starts one rrffmpeg process per transcode and probe. Each process
opened the journal, read the highest `seq` from the file tail, and counted up
from there. Nothing serialized those reads. Processes that opened together all
saw the same tail and wrote the same numbers.

Measured on the production client, 2026-11-18: the journal had 3481 lines but
only 2445 distinct `seq` values, and the largest was 2622. 1030 values were
written two or three times. The colliding lines mixed event kinds, so a
`started` for one job and a `finished` for another shared a number.

`records()` keeps one record per `seq`, which dropped 1036 lines.
`rrffmpeg-stats` then saw a `started` whose `finished` was dropped and counted
the job as open forever: `open 55` while nothing ran. The reverse, a `finished`
whose `started` was dropped, skewed duration, exit codes and the per-host
failure count. An in-memory replay uses the same rule, so the janitor also
missed events.

## Decision

Each process reserves a block of sequence numbers once, at open, under an
exclusive `flock` on `state_dir/journal.seq`. The file holds the highest
reserved number. The process reads it, takes the larger of that and the
highest `seq` on disk, writes `base + 1024`, and releases the lock. Appends
then count up from `base` in memory, as before.

`journal.seq` is not a journal file: `rotated_files()` ignores it because its
suffix is not a number. The on-disk record format does not change. Readers
keep deduplicating on `seq`, which now collapses only journals written by
0.8.0 and earlier.

The open falls back to the old tail read when the lock or the write fails: a
broken state directory must not stop a transcode.

## Consequences

- Two processes cannot take the same number, so no line is lost at merge time.
- Sequence numbers have gaps of up to 1024 between processes. Ordering by
  `seq` is still the order a reader uses; nothing assumes consecutive values.
- Opening costs one lock, one small read and one small write. Measured on
  this host: 18.5 us before, 26.7 us after, once per process. The append path
  is untouched: 1.79 us before, 1.65 us after over 2000 appends, inside noise.
- A journal written before this fix keeps its collisions. The lost lines
  cannot come back, and stale `open` counts stay until those records rotate
  out or someone deletes the journal.
- If a future defect drops a line again, `rrffmpeg-stats` now counts
  `orphan_finish` and `orphan_reaped` instead of hiding it.
- A mixed-version window can still collide: an old process reads the tail
  while a new one holds a reservation. Replace the binary on every host in
  one step.

## Alternatives rejected

- **Identity as `(seq, ts, pid)`.** It changes the record format, and old
  lines have no pid. The in-memory replay drops by `last_seq`, so it would
  still lose the second event of a collision. It treats the symptom in the
  reader.
- **Dedup on the exact line bytes when merging rotated files.** Two colliding
  lines have different bytes, so this keeps both in the statistics but
  leaves the writer producing duplicates. The live replay and the janitor
  still skip the second event, and the root cause stays.
