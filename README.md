# rrffmpeg: a remote FFmpeg wrapper for Jellyfin transcodes

[![coverage](https://codecov.io/gh/vasilvestre/rrffmpeg/branch/main/graph/badge.svg)](https://codecov.io/gh/vasilvestre/rrffmpeg)

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
- [x] Agent transport: `rrffmpeg-agent` on the worker, one TLS connection per
  job, and a second one only when the job writes `stdin`. `agent_addr` on a
  host selects it, else SSH. ADR 0009, ADR 0010, ADR 0018.
- [x] Async WAL bookkeeping plus a janitor (re-test, reap). A job that never
  started on a worker is retried once on another. A job whose client died
  still ends there: nobody is left to read its output. ADR 0013.
- [x] Per-host concurrency cap and capability matching. The cap holds across
  processes with `flock` slot files under `state_dir/coord/`, and the
  host-down verdict is shared with an expiry.
- [x] Priority across processes, and a weight per client. A waiter publishes a
  ticket per eligible host and takes a slot only when no better ticket waits:
  class first, so a probe beats a transcode, then `starts / client_weight`, so
  two clients share a busy worker in the ratio of their weights. Both need
  every client to point at the same `state_dir`. ADR 0012.
- [x] A replay harness driven by invocations recorded on the production pair.
  Six job classes came from Jellyfin 12.1.0; trickplay extraction and a
  failing probe are still hand-written. `docs/replay.md`.

Success criterion: the production pair (`client` → `worker`) runs on rrffmpeg
for a week with no manual repair. End to end, dispatch p99 stays under 50 ms
over SSH with connection persistence and under 35 ms over the agent, and the
dispatcher's own overhead stays under 10 ms at p99.

Measured so far, on one loopback box with the real ffmpeg 8.0.1: dispatch p99
32.95 ms, of which ffmpeg itself is 27.8 ms, the dispatcher adds 3.8 ms and the
TLS transport 0.4 ms. Over a LAN, persistence on SSH is 46.94 ms p99 and the
agent 33.92 ms p99 (ADR 0014). Method and raw runs in `docs/latency.md`.

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
| Client that sends bursts of short calls | Agent, or SSH with persistence | One TLS round trip, and no ssh process per call; persistence closes most of the SSH gap |
| Link with high latency or loss | Agent | One round trip for TLS 1.3, against several for SSH key exchange and auth |
| Worker with no inbound reachability | SSH | The agent needs a listening port on the worker |
| Worker that cannot keep a daemon running | SSH | Nothing to supervise on the worker |
| Worker that must not run arbitrary argv | SSH | An agent token is remote code execution by design |
| Client on a shared or spinning disk | SSH or agent, `state_dir` on tmpfs | One `fdatasync` on a contended disk can cost tens of milliseconds |

Rules that apply once a worker is configured:

1. Leave `state_dir` on the disk. Any local filesystem works: on an nvme disk a
   dispatch costs 0.5 to 2.3 ms more than on tmpfs, inside the 50 ms budget.
   Put it on tmpfs only behind a shared or spinning disk. `docs/latency.md`
   gives the numbers.
2. Never let a client without a GPU quietly transcode. After `queue_wait_secs`
   without a worker slot the process runs the local binary, unless you set
   `fallback_local = false`. Keep it false on a GPU-less client: an `ffmpeg`
   job then exits 1 and logs `no worker slot after`, while an `ffprobe` still
   falls back, because a local probe is cheap and Jellyfin needs it for
   direct-play. On a shared box the fallback runs ffmpeg at the lowest priority,
   on the wrong hardware, and can trip a memory cap.
3. A per-host key belongs in `ssh_args`, not in a shell wrapper. The global
   `ssh_args` list applies to every host, and a host's own list is appended, so
   one worker can add `-i /home/alice/.ssh/worker` while the shared options are
   written once. Both are inserted after the `ControlMaster` options and before
   the host name, so persistence keeps working. `-o` options are allowed.
4. A worker that needs variables around ffmpeg gets `[hosts.env]`, not a
   wrapper script. Each value reaches the worker literally, so a space or a `$`
   survives the remote shell. The agent transport carries the same map.
5. Capabilities come from the arguments, and you write them once per host. An
   encoder names the family and the codec (`vaapi`, `qsv`, `nvenc`, plus
   `vaapi_av1`, `qsv_av1` or `nvenc_av1` for an AV1 encoder); `main10`,
   `p010` and a `tonemap_vaapi` filter add `vaapi_hevc10` and
   `vaapi_tonemap`. All of them must match, so a 10-bit or AV1 job waits for a
   worker that can encode it instead of failing on one that only lists the
   encoder. Nothing probes on dispatch: run `rrffmpeg capabilities` on
   the client to print the exact rows, and paste them. A `-version` or
   `-encoders` probe carries no capability at all.
6. Measure before you add the agent. Time `ffprobe -version` through the
   dispatcher a few hundred times. SSH keeps one connection per host open for
   `ssh_persist_secs` (300 by default), so a job opens a channel instead of
   paying a handshake: 37.9 ms p50 on loopback, against 155.6 ms with
   persistence off. If p99 is under 50 ms, SSH is enough and the daemon is
   complexity you do not need yet. `docs/latency.md` has the runs.
7. Give every client that shares a worker the same `state_dir`, and a distinct
   `client_name`. The slot cap and the queue both live there, so a client with
   its own private `state_dir` sees a worker that is always free and a weight
   that means nothing. `client_weight` only applies while two clients wait:
   alone, a client keeps taking every slot.

```toml
client_name = "client"
client_weight = 400   # 1 to 1000, same scale as the kernel's IO weight
```

## Match Jellyfin's settings

```bash
rrffmpeg jellyfin                 # reads /etc/jellyfin/encoding.xml
rrffmpeg jellyfin --path ~/.config/jellyfin/config/encoding.xml --probe
```

The command compares the transcode settings with the `capabilities` of every
host. It reads the file and prints a table; it never writes to it, and it needs
no restart of Jellyfin.

Each Jellyfin knob only chooses the arguments Jellyfin emits, and the
dispatcher passes those through unchanged. So the audit turns the knobs into
argument sets, asks `capabilities::required` which tokens they need, and checks
who advertises them. The verdicts:

- `ok`: every host can serve it.
- `partial`: some can, and the detail names the ones that cannot. A job sent
  to the wrong host waits or fails after taking a slot, so the cost is
  capacity, not correctness.
- `broken`: no host can serve it. This is a playback path that cannot work,
  and the command exits 1.
- `unknown`: the one-frame probe cannot answer. AV1 decode and the OpenCL
  tone-mapping path are the real gaps.
- `warn`: not a routing question. `EncodingThreadCount=-1` on a host with
  `concurrency` above 1 asks for every core once per job.

It also checks two things the tokens cannot show. `VaapiDevice` must be the
device the probe tested, or every token describes another GPU: pass `--device`
to both commands. And `TranscodingTempPath` must fall under a `paths` map, or
the worker writes segments to a path it does not mount.

`--probe` re-runs the one-frame probe instead of reading the declared list, so
you see the drift after a driver or ffmpeg update. `--json` prints the same
rows for a script. Run it after any change to `encoding.xml`, to the config, or
to a worker's driver.

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
  `queue_wait_secs`. Set it false. See `Choosing a mode and a transport`.

## Evidence

Measured on 2026-11-17 on the production pair:

- Dispatch latency: 2 to 4 seconds with the state DB on a shared spinning disk
  (43% IO pressure); about 64 ms once the DB moved to tmpfs. The difference is
  the fsync on the bookkeeping commit, not SSH.
- `state_dir` on nvme against tmpfs, development box, loopback: +0.54 ms p50 on
  a fresh journal and +2.25 ms at the 8 MiB cap. The replay costs about 50 ms
  on both. See `docs/latency.md`.
- Encode throughput is GPU-bound and unaffected: 1.54x realtime on 4K HEVC HDR
  with `tonemap_opencl`, 2.67x realtime on 1080p SDR.
- A `bad`-host incident caused a full outage on a single-GPU-worker setup and
  required an external cron watchdog to clear the stale row.
- Side by side on one box, dispatch with ffmpeg replaced by `/bin/true`: 65.78
  ms p50 for rffmpeg, 7.23 ms for rrffmpeg over SSH and 1.56 ms through the
  agent. With the real `ffmpeg -version`: 117.26 ms against 33.40 and 27.77 ms,
  for a 24.18 ms local baseline. Raw runs in `docs/compare-rffmpeg.md`.
- On the production pair, same probe from the Jellyfin host to the GPU worker:
  206.4 ms p50 for rffmpeg, 52.4 ms for rrffmpeg, 30.1 ms locally. Dispatch
  alone: 155.8 ms against 32.8 ms. The journal on the host's own disk instead
  of tmpfs adds 39 ms. Raw runs in `docs/compare-rffmpeg.md`.

## Install

Each `v*` tag publishes static Linux builds for `x86_64` and `aarch64` on the
releases page, with a `.sha256` file next to each tarball. A static build needs
no Rust toolchain and no matching glibc on the Jellyfin host.

```bash
curl -fsSL https://raw.githubusercontent.com/vasilvestre/rrffmpeg/main/scripts/install.sh | sudo sh
rrffmpeg --version
```

The script checks the checksum, installs `rrffmpeg` and `rrffmpeg-stats`, links
`ffmpeg` and `ffprobe`, keeps the previous binary as `rrffmpeg.prev`, and prints
the version it installed. Run the same command to update.

No root is required. `sudo` is there only to write the default `/usr/local/bin`.
Set `PREFIX` to install as your own user:

```bash
curl -fsSL https://raw.githubusercontent.com/vasilvestre/rrffmpeg/main/scripts/install.sh | PREFIX=$HOME/.local/bin sh
```

Then point Jellyfin at `$HOME/.local/bin/ffmpeg`. That directory must be readable
by the user that runs Jellyfin, and `~/.local/bin` usually is not: many distros
set `ProtectHome=true` on the Jellyfin unit, which hides every home directory.
Use a shared path such as `/opt/rrffmpeg/bin` in that case.

On a worker that uses the agent transport, install the daemon only:

```bash
curl -fsSL https://raw.githubusercontent.com/vasilvestre/rrffmpeg/main/scripts/install.sh | sudo sh -s -- --agent
rrffmpeg-agent --version
```

`--version` answers only under the `rrffmpeg` name. The `ffmpeg` symlink keeps
passing `-version` and `--version` to the real ffmpeg, as Jellyfin expects.

The tarball is still the source of truth. To install by hand, or to read what
the script does, see [`INSTALL.md`](INSTALL.md).

The two symlinks matter: Jellyfin finds `ffprobe` next to `ffmpeg`.

The full procedure, including the agent daemon, the disk checks and a section
written for an AI agent that installs this, is in [`INSTALL.md`](INSTALL.md).

## Measure it in production

Every invocation appends to the journal, so the numbers are already on disk.
`rrffmpeg-stats` reads them back, rotated files included, and prints percentiles:

```bash
rrffmpeg-stats                      # state_dir comes from your config
rrffmpeg-stats --dir ~/.local/state/rrffmpeg --json
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
`docs/latency.md`. The side-by-side against rffmpeg is in
`docs/compare-rffmpeg.md`.

## Logging

`RRFFMPEG_LOG` sets the log level. The values are `off`, `error`, `warn`,
`info` and `debug`. The default is `warn`, so a normal run prints only warnings
and errors. Every line goes to stderr, never stdout.

```bash
RRFFMPEG_LOG=debug RRFFMPEG_CONFIG=~/.config/rffmpeg/config.toml ffmpeg -version
```

At `debug` the client prints one line per job on the agent path:
`job <id> relayed <n> B stdout, <m> B stderr in <t> ms`. Use it to see the cost
of a large passthrough such as `ffmpeg -encoders`. The SSH transport hands
inherited file descriptors to `ssh`, so it cannot count bytes and does not log
them.

## Next

```bash
cd ~/projects/oss/rrffmpeg
cargo test
cargo build --release
```

## License

MIT, see [`LICENSE`](LICENSE).
