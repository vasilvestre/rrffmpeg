# rrffmpeg: a correct dispatcher for Jellyfin transcodes

`rrffmpeg` is a Rust rewrite of [rffmpeg](https://github.com/joshuaboniface/rffmpeg).
It does one thing: it stands in for `ffmpeg` inside Jellyfin and sends each
transcode job to a machine that has the GPU. It does not touch video quality,
remuxing or encoding. Only dispatch.

The goal is not "rffmpeg in Rust". The goal is a dispatcher whose state model
is honest, whose bookkeeping never blocks the stream, and that repairs itself
instead of needing a cron watchdog.

## Why now

The existing options are all weak:

| Project | State |
|---|---|
| rffmpeg (Python) | Barely maintained. Measured defects below. |
| rffmpeg-go | Archived 2024-10-27. |
| dffmpeg (Python) | Right architecture (coordinator + worker + janitor), but beta, no job priority, no worker weight. |

At the same time the "small GPU node + separate storage/CPU host" layout is
common in home labs (N100 mini-PC with QuickSync, NAS on the other side). The
people running it have no maintained dispatcher.

## What is actually wrong with rffmpeg today

These are measured, not theoretical. Source: a production Jellyfin box (called `client`, no GPU) sending
transcodes to a worker (called `worker`, Intel N100 with VAAPI), see the
"Evidence" section.

1. **Bookkeeping sits on the critical path.** Each dispatch opens the SQLite
   DB, inserts a `processes` and a `states` row, and commits with an fsync,
   *before* `subprocess.run` launches ffmpeg:

   ```python
   log.info(f"Running command on host '{target_hostname}'")  # says "running"
   with dbconn(config) as cur:                                # ...but nothing runs yet
       cur.execute("INSERT INTO processes ...")
       cur.execute("INSERT INTO states ...")                  # fsync'd commit
   return run_command(...)                                     # the stream starts here
   ```

   On a shared disk at 43% IO pressure this cost **2 to 4 seconds per
   job**, holding back a stream that was ready to produce. An advisory write
   is gating the data path. That is the core design flaw.

2. **No reconciliation.** `get_target_host()` trusts the newest `states` row
   per host and never re-tests it. A `bad` host stays `bad` for the life of
   the process, and nothing clears it. One transient SSH timeout marked the
   only GPU host `bad` and caused a full transcode outage with no fallback
   (Jellyfin retried on `localhost`, where VAAPI does not exist). A second
   cron watchdog had to be written to repair what rffmpeg refuses to repair.

3. **Orphan jobs.** State rows are deleted only from a signal handler. `SIGKILL`
   skips it, so a killed job leaves a `bad`/`active` row behind forever. A
   dispatched ffmpeg can also outlive its session; nothing reaps it.

4. **No priority.** Every job is equal. A seek probe and a full 4K HDR
   transcode compete the same way.

5. **No real load balancing.** `weight` only divides the process count
   (`count // weight`); it is not a capacity model. No per-host concurrency
   cap, no GPU capability detection.

6. **Per-job SSH probe.** Every dispatch runs `ssh ... ffmpeg -version` as a
   health check. Jellyfin also probes capabilities at startup, one SSH round
   trip each, about 90 seconds of startup delay.

7. **No path mapping.** The worker must mount the media at the exact same
   paths, or you write a per-worker wrapper script that rewrites arguments.

## Design

Single Rust binary, used as Jellyfin's `ffmpeg` replacement. Each decision below
has a short record in [`docs/adr/`](docs/adr/README.md): context, choice, and
cost.

- **Bookkeeping off the hot path.** Job state goes to an append-only WAL
  journal with a background flusher. The transcode starts immediately; the
  bookkeeping catches up out of band. Host selection reads a fast in-memory
  counter, so async writes cannot undercount running jobs (the trap that
  breaks naive async).
- **Explicit state with reconciliation.** A janitor thread re-tests unhealthy
  hosts, reaps orphan processes, and requeues jobs when a worker disappears.
  The state model is the opposite of "newest row wins".
- **Priority queue.** Jobs carry a priority; seeks and probes do not starve
  long transcodes.
- **Capability-aware placement.** Workers advertise VAAPI/NVENC/QSV and their
  concurrency limit. The scheduler matches, instead of a global weight.
- **Transport: agent first, SSH fallback.** A tiny worker daemon removes the
  per-job SSH probe and makes retry/requeue natural. SSH stays as a
  zero-install fallback.
- **Path mapping.** Logical variables (as in dffmpeg) so client and worker can
  mount media at different roots. The host probe stats every mapped root, so a
  dropped or hung mount marks the worker down instead of failing each job.

## MVP scope (v1)

Ship the smallest thing that is strictly better than rffmpeg. State today:

- [x] Drop-in `ffmpeg`/`ffprobe` replacement with exact argument passthrough and
  correct `stdin`/`stdout`/`stderr` handling (including rffmpeg's
  `stderr_as_stdout` special-case). Verified against real ffmpeg 8.0.1.
- [x] One worker over SSH, with path mapping in both directions.
- [x] Async WAL bookkeeping plus a janitor (re-test, reap). Requeue waits for
  the agent transport: a dead client has nobody left to retry.
- [x] Per-host concurrency cap and capability matching.
- [ ] Priority across processes: the queue is per process until the daemon
  exists.
- [ ] No commit and no replay of production invocations yet. `docs/replay.md`
  explains how to record them on the client.

Success criterion: the production pair (`client` → `worker`) runs on rrffmpeg
for a week, dispatch p99 under 50 ms, and nobody has to run a repair command.

## Later

- Priority and capability-aware placement.
- Agent transport.
- Multi-worker balancing.
- A replay harness that records real Jellyfin invocations and replays them, so
  "stability" can be measured instead of felt.

## Risks

- **The Jellyfin contract is the hard part.** Argument passthrough, stream
  wiring, special flags, exit codes, ffprobe vs ffmpeg, the `EncoderPath`
  rewrite. This is weeks of testing, not the event loop.
- **Measuring stability needs a harness.** Without recorded-and-replayed real
  invocations, regressions are invisible until someone watches a film.
- **Do not reinvent the job model.** dffmpeg already solved coordinator +
  worker + heartbeat + requeue. Borrow the model; spend the effort on the
  Rust implementation and the passthrough fidelity.

## Evidence

Measured on 2026-11-17 on the production pair:

- Dispatch latency: 2 to 4 seconds with the state DB on a shared spinning disk
  (43% IO pressure); about 64 ms once the DB moved to tmpfs. The difference is
  the fsync on the bookkeeping commit, not SSH.
- Encode throughput is GPU-bound and unaffected: 1.54x realtime on 4K HEVC HDR
  with `tonemap_opencl`, 2.67x realtime on 1080p SDR.
- A `bad`-host incident caused a full outage on a single-GPU-worker setup and
  required an external cron watchdog to clear the stale row.

## Next

```bash
cd ~/projects/oss/rrffmpeg
cargo test
cargo build --release
```
