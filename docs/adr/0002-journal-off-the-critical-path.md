# 0002: Keep bookkeeping off the critical path with an append-only journal

Status: Accepted
Date: 2026-11-17

## Context

rffmpeg inserts a row and commits with an fsync before it launches ffmpeg. On a
shared disk measured at 43% IO pressure, that commit cost 2 to 4 seconds per
job. On tmpfs the same dispatch took about 64 ms. The bookkeeping gated a
stream that was ready to run.

Two traps come with any asynchronous fix:

- A naive async writer makes the scheduler undercount: the next selection
  reads the database before the write lands and starts one job too many on the
  GPU host.
- A crash mid-write must not corrupt state on the next start.

## Decision

One append-only file per state directory, one JSON object per line:

```text
{"seq":1,"ts":1763300000123,"ev":{"kind":"started","job":"42-7","host":"worker"}}
```

`append` touches only memory and an unbounded channel. It never opens a file
and never fsyncs. A single flusher thread batches queued events, writes them,
and calls `sync_data` every 500 ms or on demand. The active file rotates at
8 MiB into `journal.1`, `journal.2`, and so on.

Live state (jobs, per-host counts, down hosts) is rebuilt by replaying the
files at open. The same in-memory state is updated before the queue send, so
placement never waits for the disk. A truncated final line fails to parse and
is skipped. Duplicate sequence numbers are ignored.

## Consequences

- Dispatch latency no longer depends on the state disk. The only writer is one
  background thread.
- A crash loses at most the events still in the channel. Live counters are
  advisory: the janitor repairs what is lost.
- A write failure drops the batch with a warning instead of blocking a
  transcode. Durability is best effort, by design.
- Replay is linear in journal size. Rotation bounds the cost, and old files
  stay for the janitor and for post-mortem reading.
- There is no query language and no index. Nothing needs one at this size.
- `flush` exists for tests and shutdown. It must never be called on the
  dispatch path; it blocks until the flusher acks.

## Alternatives rejected

- SQLite with a synchronous commit. That is the measured defect.
- Memory only. The record of a SIGKILLed job would vanish with the process,
  and the janitor would have nothing to repair.
- Writing from the dispatch thread but asynchronously with a thread per event.
  More threads, same disk order, no batching.
