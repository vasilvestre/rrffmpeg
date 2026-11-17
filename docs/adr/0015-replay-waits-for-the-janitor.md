# 0015: The journal replay waits for the janitor

Status: Accepted
Date: 2026-11-17

## Context

`Journal::open` rebuilt the in-memory state by replaying every rotated file and
then the active one, line by line, before `run()` could place the job. That put
a cost proportional to the journal on disk on the dispatch path, which ADR 0002
says must stay free of bookkeeping.

Each file is capped at 8 MB, but nothing caps the number of files, as ADR 0011
already notes. The replay reads all of them, so the cost grows without bound as
the journal ages.

Measured on one machine with a synthetic journal of `started`/`finished` pairs,
60 jobs per size, `elapsed_us` read back from the journal, transport
`/bin/true`:

| Journal on disk | Dispatch p50 | Dispatch p99 |
|---|---|---|
| empty | 180 us | 208 us |
| 1 MB active file | 4.3 ms | 5.3 ms |
| 4 MB active file | 16.3 ms | 18.6 ms |
| 8 MB active file | 32.6 ms | 34.0 ms |
| 32 MB in four rotated files | 113.1 ms | 117.8 ms |

About 4 us per KB. The last row is what a few months of traffic looks like, and
it is more than twice the 50 ms dispatch budget before the job is placed.

The dispatch path does not read that state. Host selection takes occupancy from
the slot files, and the down verdict from the lock directory. Only the janitor
reads `active_jobs`, `active_on` and the journal's down set.

## Decision

- `Journal::open` reads the tail of the active file, at most 8 KB, to resume
  the sequence number. When the active file holds no record yet it reads the
  tail of the newest rotated file.
- The full replay runs on the first read that needs it, under the same mutex,
  and is done once. In practice that is the janitor's first cycle.
- The janitor sleeps one interval before its first cycle, so a job shorter
  than the interval never probes a host at all.
- `Janitor::stop` does not join a thread that is mid-cycle. A probe in flight
  is bounded by the ssh timeout, and joining it held the job's exit for
  seconds.

Both changes are guarded by tests in `state.rs` and `janitor.rs`.

## Consequences

- Dispatch cost no longer depends on the journal size: p50 133 us and p99
  196 us at an 8 MB active file, against 32.6 ms and 34.0 ms before. On a
  32 MB journal in four rotated files it goes from 113.1 ms to 132 us.
- The replay still reads every rotated file, so a long-lived process still pays
  the unbounded cost once, on the janitor thread. Pruning old rotated files is
  not done; ADR 0011 carries the same debt for `rrffmpeg-stats`.
- A job that exits inside one janitor interval never replays the journal and
  never reaps. It has nothing to reap, and the next process that lives longer
  does it.
- A host marked down by another process is retested at `every`, 10 s in the
  main path, instead of at process start. `down_ttl` is 60 s, so the verdict
  stays trusted across the wait.
- A replay that fails to read a file is retried on the next janitor cycle
  rather than failing the dispatch. The failure is silent for now; the journal
  is reported by `rrffmpeg-stats` all the same.
- The exit of a process that stops mid-probe no longer waits, so a `HostDown`
  event from that interrupted cycle can be lost. The next process retests.
- `hotpath` is now an optional dev dependency behind the `hotpath` feature.
  It is off in release, and the report goes to `HOTPATH_OUTPUT_PATH`, never to
  stdout.

## Alternatives rejected

- A snapshot file rewritten at each rotation. It bounds the replay but adds a
  second format to keep in sync, and it still costs a read on the dispatch
  path when the snapshot is large.
- A binary journal. It would cut the parse cost, not the linear read, and it
  breaks the `grep` and Loki workflows ADR 0011 relies on.
- Replaying in the flusher thread at open. It still holds the state mutex for
  the whole replay, so a dispatch that appends during it waits exactly as long.
- Dropping the replay entirely. The janitor would stop seeing jobs started by
  other processes, which is the whole point of ADR 0003.
