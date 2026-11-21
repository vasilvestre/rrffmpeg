# Why rrffmpeg exists

The goal is not "rffmpeg in Rust". The goal is a dispatcher whose state model is
honest, whose bookkeeping never blocks the stream, and that repairs itself
instead of needing a cron watchdog.

## The alternatives

| Project | State |
|---|---|
| rffmpeg (Python) | Barely maintained. Measured defects below. |
| rffmpeg-go | Archived 2024-10-27. |
| dffmpeg (Python) | Right architecture (coordinator + worker + janitor), but beta, no job priority, no worker weight. |

At the same time the "small GPU node + separate storage/CPU host" layout is
common in home labs (N100 mini-PC with QuickSync, NAS on the other side). The
people running it have no maintained dispatcher.

## What is actually wrong with rffmpeg today

These are measured, not theoretical. Source: a production Jellyfin box (called
`client`, no GPU) sending transcodes to a worker (called `worker`, Intel N100
with VAAPI), see the "Evidence" section.

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

## How rrffmpeg answers

Single Rust binary, used as Jellyfin's `ffmpeg` replacement. Each decision below
has a short record in [`adr/`](adr/README.md): context, choice, and cost.

- **Bookkeeping off the hot path.** Job state goes to an append-only WAL
  journal with a background flusher. The transcode starts immediately; the
  bookkeeping catches up out of band. Host capacity lives in the state dir, not
  in this process, so a forked job still sees it.
- **Explicit state with reconciliation.** A janitor thread re-tests unhealthy
  hosts, reaps orphan processes, and requeues jobs when a worker disappears.
  The state model is the opposite of "newest row wins".
- **Priority, then weight, across processes.** A waiting job publishes a
  ticket under `state_dir/coord/<host>/queue/` and declines any slot a better
  ticket is waiting for. A probe outranks a transcode, and a client with
  `client_weight = 400` starts four times as often as one at 100 while both
  wait. A client alone still takes every slot. ADR 0012.
- **Capability-aware placement.** Workers advertise VAAPI/NVENC/QSV and their
  concurrency limit. The scheduler matches, instead of a global weight. The
  limit is enforced by one exclusive lock file per slot, freed by the kernel
  when the process dies.
- **Transport: agent, or SSH fallback, per host.** A tiny worker daemon removes
  the per-job SSH handshake and the per-job probe, and tells the client which
  jobs are alive. A host without `agent_addr` keeps using SSH, which needs
  nothing installed.
- **Path mapping.** Logical variables (as in dffmpeg) so client and worker can
  mount media at different roots. The host probe stats every mapped root, so a
  dropped or hung mount marks the worker down instead of failing each job.

## Evidence

Measured on 2026-11-17 on the production pair:

- Dispatch latency: 2 to 4 seconds with the state DB on a shared spinning disk
  (43% IO pressure); about 64 ms once the DB moved to tmpfs. The difference is
  the fsync on the bookkeeping commit, not SSH.
- `state_dir` on nvme against tmpfs, development box, loopback: +0.54 ms p50 on
  a fresh journal and +2.25 ms at the 8 MiB cap. The replay costs about 50 ms
  on both. See `latency.md`.
- Encode throughput is GPU-bound and unaffected: 1.54x realtime on 4K HEVC HDR
  with `tonemap_opencl`, 2.67x realtime on 1080p SDR.
- A `bad`-host incident caused a full outage on a single-GPU-worker setup and
  required an external cron watchdog to clear the stale row.
- Side by side on one box, dispatch with ffmpeg replaced by `/bin/true`: 65.78
  ms p50 for rffmpeg, 7.23 ms for rrffmpeg over SSH and 1.56 ms through the
  agent. With the real `ffmpeg -version`: 117.26 ms against 33.40 and 27.77 ms,
  for a 24.18 ms local baseline. Raw runs in `compare-rffmpeg.md`.
- On the production pair, same probe from the Jellyfin host to the GPU worker:
  206.4 ms p50 for rffmpeg, 52.4 ms for rrffmpeg, 30.1 ms locally. Dispatch
  alone: 155.8 ms against 32.8 ms. The journal on the host's own disk instead
  of tmpfs adds 39 ms. Raw runs in `compare-rffmpeg.md`.

## Next

Repeat the comparison on your own pair:

```bash
rrffmpeg-stats
python3 scripts/measure.py 200 ffprobe -version
```
