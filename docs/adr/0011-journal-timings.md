# 0011: The journal carries its own timings

Status: Accepted
Date: 2026-11-17

## Context

The dispatch figure that matters in production is "how long did rrffmpeg take
before the worker started ffmpeg", and the only measurement so far came from a
benchmark on one machine (`docs/latency.md`). The success criterion is a week
of real traffic with dispatch p99 under 50 ms, so the number has to come out of
the running pair.

The usual answer is a metrics endpoint. It does not fit here. rrffmpeg is one
process per job: it lives for tens of milliseconds and exits. There is no port
to scrape and no counter that survives between jobs, so a pull-based exporter
would have to be a second daemon on the client, fed by the same data.

The journal already records every event with a timestamp, and the site already
ships logs to Loki with Alloy. What it lacked was resolution and a start point:

- `ts` was wall-clock milliseconds. A dispatch costs a fraction of a
  millisecond, so a millisecond field would round it to 0 or 1.
- `Started` carried nothing about the process that wrote it, so the interval
  from process start to dispatch was not observable at all.

## Decision

- `Record.ts` is wall-clock microseconds instead of milliseconds.
- `Event::Started` gains `elapsed_us`: microseconds from process start to the
  append, read from a monotonic clock so a clock step cannot corrupt it.
- A new module `stats` and a new binary `rrffmpeg-stats` read the journal back,
  including rotated `journal.N` files, and print percentiles of dispatch and
  job duration. Text by default, `--json` for anything machine-driven.

The two clocks stay separate on purpose: `ts` orders events across processes
and hosts, `elapsed_us` measures one process against itself.

## Consequences

- The journal format changed. A file written by an older binary still parses,
  because no field was removed, but its `ts` values are milliseconds and its
  durations read as microseconds. Nothing upgrades in place, so the only cost
  is a mixed journal after an upgrade, which lasts one rotation.
- Nothing new runs. `rrffmpeg-stats` reads a file and exits, so it can be a
  cron entry, a shell command, or a Loki tail on the same path. Alloy needs one
  mount and one target, no stage.
- What `elapsed_us` covers and does not: it stops when the job is handed to the
  transport. Config, the journal replay and the slot file are inside it. The
  TLS handshake, the daemon spawn and rrffmpeg's own exit are outside.
- Percentiles come from nearest rank over a sample count, so p99 of a 13 job
  journal is just the maximum. Read it with the `n=` in the same line.
- The recorder keeps every record in memory to dedupe by `seq`. Rotation caps
  one file at 8 MiB but nothing caps the number of files, so a journal left
  alone for months makes this binary read all of it.
- A job that starts and never finishes counts as open, not as a sample, so a
  crash cannot drag the duration down.

## Alternatives rejected

- A Prometheus endpoint or an OTLP push. Both need a process that outlives the
  job. The agent daemon could expose counters, but it sees only its own side of
  the trip, not the dispatch cost on the client.
- A dedicated metrics file next to the journal, in the shape of the `psi.log`
  the homelab already collects. It duplicates what the journal holds, and the
  two files drift.
- Emitting one summary line per job to stderr. Jellyfin owns that stream on a
  transcode, and the project already refuses to write anything there.
- Parsing the journal in a shell one-liner instead of a binary. Possible for a
  mean, not for percentiles across rotated files, and not for open jobs.
