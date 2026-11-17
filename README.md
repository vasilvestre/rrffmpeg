# rrffmpeg: a remote FFmpeg wrapper for Jellyfin transcodes

`rrffmpeg` is a remote FFmpeg wrapper in Rust. Like
[rffmpeg](https://github.com/joshuaboniface/rffmpeg), it stands in for `ffmpeg`
inside Jellyfin and sends each transcode command to a machine that has the GPU,
over SSH or through the `rrffmpeg-agent` daemon. Unlike rffmpeg, dispatch does
not wait on an fsync, a dead host is retested instead of marked bad forever,
and the orphans `SIGKILL` leaves behind get reaped.

It does not touch video quality, remuxing or encoding. Only dispatch.

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

## MVP scope (v1)

Ship the smallest thing that is strictly better than rffmpeg. State today:

- [x] Drop-in `ffmpeg`/`ffprobe` replacement with exact argument passthrough and
  correct `stdin`/`stdout`/`stderr` handling (including rffmpeg's
  `stderr_as_stdout` special-case). Verified against real ffmpeg 8.0.1.
- [x] One worker over SSH, with path mapping in both directions.
- [x] Agent transport: `rrffmpeg-agent` on the worker, two TLS connections per
  job. `agent_addr` on a host selects it, else SSH. ADR 0009 and ADR 0010.
- [x] Async WAL bookkeeping plus a janitor (re-test, reap). Requeue waits for
  the agent transport: a dead client has nobody left to retry.
- [x] Per-host concurrency cap and capability matching. The cap holds across
  processes with `flock` slot files under `state_dir/coord/`, and the
  host-down verdict is shared with an expiry.
- [x] Priority across processes, and a weight per client. A waiter publishes a
  ticket per eligible host and takes a slot only when no better ticket waits:
  class first, so a probe beats a transcode, then `starts / client_weight`, so
  two clients share a busy worker in the ratio of their weights. Both need
  every client to point at the same `state_dir`. ADR 0012.
- [ ] No commit and no replay of production invocations yet. `docs/replay.md`
  explains how to record them on the client.

Success criterion: the production pair (`client` → `worker`) runs on rrffmpeg
for a week, dispatch p99 under 50 ms, and nobody has to run a repair command.

Measured so far, on one loopback box with the real ffmpeg 8.0.1: dispatch p99
32.95 ms, of which ffmpeg itself is 27.8 ms, the dispatcher adds 3.8 ms and the
TLS transport 0.4 ms. Method and raw runs in `docs/latency.md`.

## Agent transport

SSH needs nothing on the worker. The agent removes the per-job handshake and
the per-job probe, and it answers "is this job still running" from memory. It
speaks TLS, and the client trusts exactly one certificate: the daemon's own.

Generate the pair on the worker. List every name and address a client will use
to reach it, because the certificate is checked against `agent_addr`:

```bash
install -D rrffmpeg-agent ~/.local/bin/rrffmpeg-agent
rrffmpeg-agent --init ~/.config/rrffmpeg --san worker --san 10.8.0.2
```

That writes `cert.pem` and `key.pem`, the key mode 0600. Start the daemon:

```bash
RRFFMPEG_AGENT_TOKEN=change-me rrffmpeg-agent \
    --listen 10.8.0.2:9876 \
    --cert ~/.config/rrffmpeg/cert.pem \
    --key ~/.config/rrffmpeg/key.pem
```

Copy `cert.pem` to the client. Copy it over a channel you already trust, since
it is the only thing standing between the client and an impostor. Then:

```toml
agent_token = "change-me"
agent_cert = "/home/alice/.config/rrffmpeg/worker-cert.pem"

[[hosts]]
name = "worker"
agent_addr = "worker:9876"
```

The address may differ from `name`. A host without `agent_addr` keeps using
SSH, so one config can mix both transports.

The certificate proves the daemon to the client. The token proves the client to
the daemon; it travels inside TLS, but a stolen token still works from any host
that can reach the address. Keep the listener on a private network until mTLS
exists.

## Choosing a mode and a transport

Pick by where the dispatcher runs, not by preference. The cost of SSH is one
process and one key exchange per job. The agent removes that cost and adds a
daemon on the worker.

| Where the dispatcher runs | Configuration | Reason |
|---|---|---|
| The box has the GPU | no `[[hosts]]` | Local mode: no state, no thread, no disk |
| Client without a GPU, LAN, a few jobs per hour | SSH | Nothing to install, and the cost is invisible at that rate |
| Client that sends bursts of short calls | Agent | Every Jellyfin startup probe is one SSH handshake otherwise |
| Link with high latency or loss | Agent | One round trip for TLS 1.3, against several for SSH key exchange and auth |
| Worker with no inbound reachability | SSH | The agent needs a listening port on the worker |
| Worker that cannot keep a daemon running | SSH | Nothing to supervise on the worker |
| Worker that must not run arbitrary argv | SSH | An agent token is remote code execution by design |
| Any client on a slow or contended disk | SSH or agent, `state_dir` on tmpfs | Every invocation replays the journal before it dispatches |

Rules that apply once a worker is configured:

1. Put `state_dir` on tmpfs when the disk is slow. Each job is a new process,
   so each dispatch reads the journal back. `Evidence` below gives the measure.
2. Never let a client without a GPU quietly transcode. After `queue_wait_secs`
   without a worker slot the process runs the local binary, and there is no
   switch to turn that off. Raise `queue_wait_secs`, and watch the log for
   `rrffmpeg: no worker slot after`. On a shared box the fallback runs ffmpeg
   at the lowest priority, on the wrong hardware, and can trip a memory cap.
3. Capability matching only reads encoder flags. A `-version` or `-encoders`
   probe carries no capability, so with several workers it may land on one
   that cannot encode. With a single worker it does not matter.
4. Measure before you add the agent. Time `ffprobe -version` through the
   dispatcher a few hundred times. If p99 is under 50 ms, SSH is enough and
   the daemon is complexity you do not need yet.
5. Give every client that shares a worker the same `state_dir`, and a distinct
   `client_name`. The slot cap and the queue both live there, so a client with
   its own private `state_dir` sees a worker that is always free and a weight
   that means nothing. `client_weight` only applies while two clients wait:
   alone, a client keeps taking every slot.

```toml
client_name = "client"
client_weight = 400   # 1 to 1000, same scale as the kernel's IO weight
```

## Later

- Capability-aware placement that reads more than encoder flags.
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
- **The journal rotates at 8 MiB but nothing prunes it.** That is fine on a
  disk and unbounded on tmpfs, which is exactly where a slow disk pushes you.
  Watch the directory size, or add a retention rule before the client lives on
  tmpfs for months.
- **A full queue falls back to local execution.** The fallback always runs,
  with no config switch, so a GPU-less client can end up transcoding on the
  wrong hardware. See `Choosing a mode and a transport`.

## Evidence

Measured on 2026-11-17 on the production pair:

- Dispatch latency: 2 to 4 seconds with the state DB on a shared spinning disk
  (43% IO pressure); about 64 ms once the DB moved to tmpfs. The difference is
  the fsync on the bookkeeping commit, not SSH.
- Encode throughput is GPU-bound and unaffected: 1.54x realtime on 4K HEVC HDR
  with `tonemap_opencl`, 2.67x realtime on 1080p SDR.
- A `bad`-host incident caused a full outage on a single-GPU-worker setup and
  required an external cron watchdog to clear the stale row.

## Install

Each `v*` tag publishes static Linux builds for `x86_64` and `aarch64` on the
releases page, with a `.sha256` file next to each tarball. A static build needs
no Rust toolchain and no matching glibc on the Jellyfin host.

```bash
curl -LO https://github.com/vasilvestre/rrfmpeg/releases/latest/download/rrffmpeg-x86_64-unknown-linux-musl.tar.gz
sha256sum -c rrffmpeg-x86_64-unknown-linux-musl.tar.gz.sha256
tar xzf rrffmpeg-x86_64-unknown-linux-musl.tar.gz
sudo install -m 755 rrffmpeg-x86_64-unknown-linux-musl/rrffmpeg /usr/local/bin/rrffmpeg
sudo ln -sf /usr/local/bin/rrffmpeg /usr/local/bin/ffmpeg
sudo ln -sf /usr/local/bin/rrffmpeg /usr/local/bin/ffprobe
```

The two symlinks matter: Jellyfin finds `ffprobe` next to `ffmpeg`.

## Measure it in production

Every invocation appends to the journal, so the numbers are already on disk.
`rrffmpeg-stats` reads them back, rotated files included, and prints percentiles:

```bash
rrffmpeg-stats                      # state_dir comes from your config
rrffmpeg-stats --dir /dev/shm/rrffmpeg --json
```

```text
journal /home/alice/.local/state/rrffmpeg
records 26  jobs 13  open 0  reaped 0
dispatch  n=13    p50 217 us    p95 276 us    p99 276 us    max 276 us
duration  n=13    p50 26.25 ms  p95 29.21 ms  p99 29.21 ms  max 29.21 ms
exit codes  0:13
host worker  jobs 13  failed 0  down 0  up 0
```

`dispatch` is microseconds from process start to the moment the job went to the
worker: config, the journal replay and the slot file. The transport, the daemon
and rrffmpeg's own exit are outside it. `duration` is the gap between the
`started` and `finished` events, so it is the job itself.

What it does not measure: the delay Jellyfin sees before the first frame, and
anything on the worker beyond the process lifetime. For the week-long test, run
it from cron and keep the output, or point a log collector at the journal: it is
one JSON object per line, and `--json` gives the same figures in one object.

The benchmark behind the transport numbers, with its method and raw runs, is in
`docs/latency.md`.

## Next

```bash
cd ~/projects/oss/rrffmpeg
cargo test
cargo build --release
```
