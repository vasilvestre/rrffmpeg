# 0026: A journal block is exclusive, and replay never moves the writer

Status: Accepted
Date: 2026-11-18

## Context

ADR 0025 reserved a block of 1024 sequence numbers per process at open. That
was half the fix. `replay()` still ended with `live.seq = live.seq.max(record.seq)`,
so the first read of the journal moved the writer counter to the highest
number on disk. That number belongs to another process's block.

Every process reads before it writes: the dispatcher calls `active_on` for
`in_flight` just before appending `Started`, and the janitor replays at start.
Two processes that replayed the same tail then appended the same number.
Measured on the development host with the 0.8.1 release binary, 20 concurrent
probes, 10 runs: 40 lines each time, but 33 to 39 distinct `seq` values. The
same test with the 0.8.0 binary failed every run.

A second hole: a process that wrote more than 1024 records kept counting past
its block, because nothing renewed it. A long-lived process would walk into
the next block.

## Decision

Two rules make the block exclusive.

1. `replay()` never writes `live.seq`. It applies events and deduplicates with
   a `HashSet` of the `seq` values seen in that replay, one set across the
   rotated files and the active journal. The first line wins for a journal
   written by 0.8.0 or earlier, which is what the old `last_seq` filter did.
2. `append()` checks `seq >= block_end` before taking the next number. When
   the block is full it reserves the next one under the same `flock` on
   `journal.seq`, then continues. This costs one lock per 1024 records, not
   one per append.

The on-disk format does not change. `journal.seq` is still outside the
rotated set, and readers still deduplicate on `seq`.

## Consequences

- Two processes cannot take the same number, at open or after 1024 appends.
- Replay uses memory proportional to the journal records it reads. At the
  8 MB rotation cap that is tens of thousands of `u64` keys, once per process,
  off the dispatch path.
- Appending measured 1.26 us on 0.8.1 and 1.44 us here over 2000 appends,
  including the renewal. The dispatch budget is 1 ms, so this is noise.
- Gaps between blocks stay bounded by 1024 per block. `seq` stays the reader's
  order key; nothing assumes consecutive values.
- Journals written by 0.8.0 keep their collisions, and a journal written by
  0.8.1 keeps the duplicates its replay created. Neither can be repaired. The
  orphan counters in `rrffmpeg-stats` show the old damage; deleting the
  journal clears the stale `open` count.
- A mixed-version window can still collide when an old process reads the tail
  while a new one holds a reservation. Replace the binary in one step.

## Alternatives rejected

- **Identity as `(seq, ts, pid)`.** It changes the format, old lines have no
  pid, and the live replay would still need a rule for duplicates. It fixes
  the reader, not the writer.
- **Dedup on exact line bytes when merging rotated files.** Two colliding
  lines have different bytes, so statistics keep both, but the writer still
  emits duplicates and the janitor still replays one event twice or drops
  one.
- **Renew the block only when it is exhausted, and keep the replay jump.**
  Renewal alone would also prevent a collision, because a jumped counter is
  always past `block_end`. But the counter would no longer mean "my block",
  and the next change to replay could reintroduce the bug. The invariant is
  cheaper to keep than to re-derive.
- **A global counter locked on every append.** It serialises every dispatch
  through one lock and one file write, which is the cost ADR 0025 removed
  from the dispatch path.
