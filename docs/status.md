# Status

Last updated 2026-11-21, version 0.16.0.

## What works (v1)

Ship the smallest thing that is strictly better than rffmpeg. State today:

- [x] Drop-in `ffmpeg`/`ffprobe` replacement with exact argument passthrough and
  correct `stdin`/`stdout`/`stderr` handling (including rffmpeg's
  `stderr_as_stdout` special-case). Verified against real ffmpeg 8.0.1.
- [x] One worker over SSH, with path mapping in both directions.
- [x] Agent transport: `rrffmpeg-agent` on the worker, one TLS connection per
  job, and a second one only when the job writes `stdin`. `agent_addr` on a
  host selects it, else SSH. ADR 0009, ADR 0010, ADR 0018.
- [x] Async WAL bookkeeping plus a janitor (re-test, reap). A job that never
  started on a worker is retried once on another. A job whose client died
  still ends there: nobody is left to read its output. ADR 0013.
- [x] Per-host concurrency cap and capability matching. The cap holds across
  processes with `flock` slot files under `state_dir/coord/`, and the
  host-down verdict is shared with an expiry.
- [x] Two pools per host: `concurrency` for transcodes, `light_concurrency`
  (default 2) for the work Jellyfin needs to finish in seconds, which is
  `ffprobe`, a single thumbnail, an MJPEG sprite, a `-skip_frame nokey`
  trickplay scan and a startup listing (`-version`, `-encoders`, `-hwaccels`,
  `-filters`, `-h`). A Light job never waits behind a transcode, and when the
  light pool is full on a `local = true` host it runs on the client at once
  instead of waiting. ADR 0032.
- [x] Caps sized from the journal. `concurrency = "auto"` (and an absent key)
  takes the 5th percentile of that host's own solo
  speeds for the content class, divides by 1.3, and clamps to
  `[1, min(4, cpus / 2)]`. Ten finishes at 1.3x or better raise it by one,
  three under 1.0x lower it by one, both derived from the same tail with no
  stored counter. A job that stops using the CPU and stops writing is parked:
  it keeps its slot but stops counting toward the cap, so a paused stream does
  not look like a slow GPU. Zero and `unlimited` are a config error. ADR 0033.
- [x] Priority across processes, and a weight per client. A waiter publishes a
  ticket per eligible host and takes a slot only when no better ticket waits:
  class first, so a probe beats a transcode, then `starts / client_weight`, so
  two clients share a busy worker in the ratio of their weights. Both need
  every client to point at the same `state_dir`. ADR 0012.
- [x] A replay harness driven by invocations recorded on the production pair.
  Six job classes came from Jellyfin 12.1.0; trickplay extraction and a
  failing probe are still hand-written. `replay.md`.

Success criterion: the production pair (`client` → `worker`) runs on rrffmpeg
for a week with no manual repair. End to end, dispatch p99 stays under 50 ms
over SSH with connection persistence and under 35 ms over the agent, and the
dispatcher's own overhead stays under 10 ms at p99.

Measured so far, on one loopback box with the real ffmpeg 8.0.1: dispatch p99
32.95 ms, of which ffmpeg itself is 27.8 ms, the dispatcher adds 3.8 ms and the
TLS transport 0.4 ms. Over a LAN, persistence on SSH is 46.94 ms p99 and the
agent 33.92 ms p99 (ADR 0014). Method and raw runs in `latency.md`.

## Later

- Multi-worker balancing.
- A recording of trickplay extraction and of a failing probe. The other job
  classes are already replayed from real invocations.

## Risks

- **The Jellyfin contract is the hard part.** Argument passthrough, stream
  wiring, special flags, exit codes, ffprobe vs ffmpeg, the `EncoderPath`
  rewrite. This is weeks of testing, not the event loop.
- **Measuring stability needs a harness, and the harness has a blind spot.** The
  replay test compares argv, exit codes and stream wiring against recorded real
  invocations, so a passthrough regression fails before a viewer sees it. It
  cannot see a wrong filter chain, a GPU failure or a dispatch bug: the stand-in
  never encodes. Use `tests/real_ffmpeg.rs` and the production pair for those.
- **Do not reinvent the job model.** dffmpeg already solved coordinator +
  worker + heartbeat + requeue. Borrow the model; spend the effort on the
  Rust implementation and the passthrough fidelity.
- **The journal rotates at 8 MiB but nothing prunes it.** A replay at the cap
  costs about 50 ms whatever the filesystem, which is the whole budget. That,
  not the disk, is what pushes people to tmpfs. Keep the journal small, or move
  the replay off the critical path.
- **A forgotten fallback still encodes locally.** `fallback_local` defaults to
  true so a v0.5.0 config keeps working, which means a GPU-less client that
  never sets it false still transcodes on the wrong hardware after
  `queue_wait_secs`. Set it false. See [`configuration.md`](configuration.md).

## Next

Check the suite and the release notes before you change a design choice:

```bash
cargo test
ls docs/releases
```
