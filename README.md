# rrffmpeg: a remote FFmpeg wrapper for Jellyfin transcodes

[![coverage](https://codecov.io/gh/vasilvestre/rrffmpeg/branch/main/graph/badge.svg)](https://codecov.io/gh/vasilvestre/rrffmpeg)

Jellyfin calls `ffmpeg`. rrffmpeg answers, and runs the job on the machine that
has the GPU, over SSH or through its own agent daemon. Arguments, standard
input, standard output and standard error pass through unchanged.

It removes the three defects that made the Python rffmpeg painful: a bookkeeping
commit that sat before the transcode and cost 2 to 4 seconds per job on a
contended disk, a dead worker marked bad for the life of the process, and orphan
rows left by `SIGKILL`. rrffmpeg keeps its state off the hot path and repairs
itself, with no cron watchdog.

It does not touch video quality, remuxing or encoding. Only dispatch.

```bash
curl -fsSL https://raw.githubusercontent.com/vasilvestre/rrffmpeg/main/scripts/install.sh | sudo sh
rrffmpeg --version
```

## Install

Each `v*` tag publishes static Linux builds for `x86_64` and `aarch64`, with a
`.sha256` file next to each tarball. A static build needs no Rust toolchain and
no matching glibc on the Jellyfin host.

The script checks the checksum, installs `rrffmpeg` and `rrffmpeg-stats`, links
`ffmpeg` and `ffprobe`, keeps the previous binary as `rrffmpeg.prev`, and prints
the version it installed. Run the same command to update.

`sudo` is there only to write the default `/usr/local/bin`. Set `PREFIX` to
install as your own user:

```bash
curl -fsSL https://raw.githubusercontent.com/vasilvestre/rrffmpeg/main/scripts/install.sh | PREFIX=$HOME/.local/bin sh
```

Jellyfin must be able to read that directory. Many distros set
`ProtectHome=true` on the Jellyfin unit, which hides every home directory, so
use a shared path such as `/opt/rrffmpeg/bin` in that case.

On a worker that uses the agent transport, install the daemon only:

```bash
curl -fsSL https://raw.githubusercontent.com/vasilvestre/rrffmpeg/main/scripts/install.sh | sudo sh -s -- --agent
rrffmpeg-agent --version
```

The two symlinks matter: Jellyfin finds `ffprobe` next to `ffmpeg`. The
`--version` flag answers only under the `rrffmpeg` name; the `ffmpeg` symlink
keeps passing `-version` and `--version` to the real ffmpeg, as Jellyfin
expects.

The full procedure, including the systemd unit for the agent, the GPU and disk
checks, and a section written for an AI agent, is in [`INSTALL.md`](INSTALL.md).
The tarball stays the source of truth.

## Configure

The client reads `$RRFFMPEG_CONFIG`, else
`$XDG_CONFIG_HOME/rffmpeg/config.toml`, else `~/.config/rffmpeg/config.toml`.
With no config file, rrffmpeg runs the local `ffmpeg` and does nothing else.
This file sends every job to one worker over SSH:

```toml
state_dir = "/home/alice/.local/state/rrffmpeg"
local_ffmpeg = "/usr/bin/ffmpeg"
local_ffprobe = "/usr/bin/ffprobe"
fallback_local = false

[[hosts]]
name = "worker"
concurrency = "auto"
capabilities = ["vaapi"]
ssh_args = ["-i", "/home/alice/.ssh/worker"]

[[hosts.paths]]
from = "/home/alice"
to = "/home/bob/media"
```

- `name` is the SSH destination: `worker`, `user@worker`, or an address.
- `concurrency` is the number of jobs the worker accepts at once, or `"auto"`
  to size it from that worker's own journal. `"auto"` starts at 1 and climbs
  one job per ten finishes at 1.3x realtime or better; a number pins it.
- `capabilities` lists the accelerators the worker really has. Run
  `rrffmpeg capabilities worker` to print the rows to paste.
- `[[hosts.paths]]` rewrites the input and the output roots, longest `from`
  first, in both directions.
- `fallback_local = false` stops an `ffmpeg` job from running on a GPU-less
  client when no worker slot frees up. Keep the default `true` only on a client
  that has the GPU.

`config.example.toml` is the full client and worker pair, with every key
commented. [`docs/configuration.md`](docs/configuration.md) covers the agent
transport, the mode table, capabilities and the audit of Jellyfin's settings.

## Why it exists

The alternatives are all weak. rffmpeg is barely maintained, rffmpeg-go was
archived in 2024, and dffmpeg has the right architecture but is beta, with no
priority and no worker weight. These are the measured defects rrffmpeg removes:

- **Bookkeeping on the critical path.** A SQLite fsync sat between "decide to
  run" and "run ffmpeg": 2 to 4 seconds per job on a disk at 43% IO pressure,
  against about 64 ms on tmpfs. Job state now goes to a journal with a
  background flusher, so the stream starts at once.
- **No reconciliation.** One transient SSH timeout marked the only GPU host
  `bad` forever, and an external cron watchdog had to clear it. A janitor
  thread now re-tests unhealthy hosts, reaps orphans and requeues their jobs.
- **Orphans.** `SIGKILL` skipped the signal handler and left state rows
  forever. The janitor reaps them.
- **No priority, no real balancing, no path mapping, one SSH probe per job.**
  All four are in: a ticket queue with a weight per client, capability matching
  under a per-host slot cap, path mapping in both directions, and one
  persistent connection per host.

The proof, with the code and the raw runs, is in [`docs/why.md`](docs/why.md).

## Status

v1 works: drop-in `ffmpeg`/`ffprobe`, SSH and agent transports, path mapping,
two pools per host with caps sized from the journal, priority with a weight per
client, capability matching, and self-repair after a kill or a dead worker.

Dispatch p99 is 32.95 ms on loopback with the real ffmpeg 8.0.1, against a 50 ms
budget, of which the dispatcher adds 3.8 ms. Over a LAN, 46.94 ms p99 on SSH
with persistence and 33.92 ms through the agent.

Open: multi-worker balancing. Known risks and the full checklist are in
[`docs/status.md`](docs/status.md).

## Docs

| Document | What it holds |
|---|---|
| [`INSTALL.md`](INSTALL.md) | Client and worker install, the agent daemon, GPU and disk checks |
| [`docs/configuration.md`](docs/configuration.md) | Transports, modes, placement, capabilities, the Jellyfin audit |
| [`docs/why.md`](docs/why.md) | The alternatives, the measured defects of rffmpeg, and the evidence |
| [`docs/status.md`](docs/status.md) | What works, what is next, known risks |
| [`docs/operations.md`](docs/operations.md) | `rrffmpeg-stats`, logging, the latency budget |
| [`docs/adr/`](docs/adr/README.md) | Decision records: context, choice, cost |
| [`docs/latency.md`](docs/latency.md) | Dispatch latency method and raw runs |
| [`docs/compare-rffmpeg.md`](docs/compare-rffmpeg.md) | Side by side against rffmpeg |
| [`docs/replay.md`](docs/replay.md) | Replay harness from recorded Jellyfin calls |
| [`config.example.toml`](config.example.toml) | The full client and worker pair |

## Next

```bash
git clone https://github.com/vasilvestre/rrffmpeg && cd rrffmpeg
cargo test
```

## License

MIT, see [`LICENSE`](LICENSE).
