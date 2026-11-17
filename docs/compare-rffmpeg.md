# rffmpeg against rrffmpeg, one box

How much does each dispatcher add between "Jellyfin calls ffmpeg" and "ffmpeg
starts"? The project pitch says rffmpeg waits on a database commit and probes
the worker on every job. This page puts both programs through the same harness
on the same machine and shows the cost.

Every row was produced with the setup below, in one session, on 2026-11-17.
Measurements are milliseconds. Percentiles are nearest rank.

## What is compared

| Program | Version | Transport |
|---|---|---|
| rffmpeg | `f54ac84`, 2025-11-03 | SSH, one connection per job with persistence |
| rrffmpeg | 0.3.0 | SSH, `ssh_persist_secs = 300` |
| rrffmpeg | 0.3.0 | `rrffmpeg-agent` on `127.0.0.1:9876` |

All four send the job to `127.0.0.2`, an address served by the sshd on the same
box. rffmpeg runs its own `ffmpeg` symlink from a checkout, rrffmpeg runs the
release binary. Both reach `/usr/bin/ffmpeg` 8.0.1 on the worker side.

The job is `-version`, first with the real ffmpeg, then with `/bin/true` in
place of ffmpeg to remove codec work and keep only the dispatch.

## Machine

| | |
|---|---|
| CPU | 11th Gen Intel Core i7-11800H, 2.30 GHz, 16 threads |
| RAM | 31 812 MiB |
| Kernel | 7.2.15-76070215-generic |
| Load during the runs | 0.88 to 1.12 on 16 threads |
| Network | loopback only, sshd on the same box |

## Method

One harness for all rows: `scripts/measure.py`. It drops 5 warmup runs, keeps
60, times each with `time.perf_counter`, and sends stdin, stdout and stderr to
`/dev/null`. The SSH control master is already open, so every row measures the
steady state and not the first connection. rffmpeg keeps its persistence at the
default 300 s.

```bash
cargo build --release
cp /path/to/rffmpeg-checkout/rffmpeg /tmp/bench/bin/rffmpeg
ln -s rffmpeg /tmp/bench/bin/ffmpeg
```

The rffmpeg config for one host, with the state directory on the filesystem
under test:

```yaml
rffmpeg:
  logging:
    log_to_file: true
    logfile: /tmp/bench/nvme/rffmpeg.log
  directories:
    state: /tmp/bench/nvme
    persist: /tmp/bench/persist
  remote:
    user: alice
    persist: 300
    args: []
  commands:
    ssh: /usr/bin/ssh
    ffmpeg: /usr/bin/ffmpeg
    ffprobe: /usr/bin/ffprobe
    fallback_ffmpeg: /usr/bin/ffmpeg
    fallback_ffprobe: /usr/bin/ffprobe
```

```bash
RFFMPEG_CONFIG=/tmp/bench/rffmpeg-nvme.yml /tmp/bench/bin/rffmpeg init --no-root -y
RFFMPEG_CONFIG=/tmp/bench/rffmpeg-nvme.yml /tmp/bench/bin/rffmpeg add 127.0.0.2
```

The rrffmpeg side, one config per transport:

```toml
state_dir = "/tmp/bench/rr-ssh-state"
ssh_persist_secs = 300

[[hosts]]
name = "127.0.0.2"
concurrency = 1
ffmpeg_bin = "/usr/bin/ffmpeg"
```

```toml
state_dir = "/tmp/bench/rr-agent-state"
agent_cert = "/tmp/bench/agent/cert.pem"
agent_token = "bench-token"

[[hosts]]
name = "worker"
concurrency = 1
agent_addr = "127.0.0.1:9876"
ffmpeg_bin = "/usr/bin/ffmpeg"
```

```bash
printf 'local ffmpeg          '; python3 scripts/measure.py 60 /usr/bin/ffmpeg -version
printf 'rffmpeg nvme          '; RFFMPEG_CONFIG=/tmp/bench/rffmpeg-nvme.yml python3 scripts/measure.py 60 /tmp/bench/bin/ffmpeg -version
printf 'rffmpeg tmpfs         '; RFFMPEG_CONFIG=/tmp/bench/rffmpeg-tmpfs.yml python3 scripts/measure.py 60 /tmp/bench/bin/ffmpeg -version
printf 'rrffmpeg ssh          '; RRFFMPEG_CONFIG=/tmp/bench/rr-ssh.toml python3 scripts/measure.py 60 ./target/release/rrffmpeg -version
printf 'rrffmpeg agent        '; RRFFMPEG_CONFIG=/tmp/bench/rr-agent.toml python3 scripts/measure.py 60 ./target/release/rrffmpeg -version
```

## Dispatch only

Ffmpeg is `/bin/true`, so nothing encodes. Release binaries, warm SSH, n=60.

| Row | p50 | p95 | p99 | Added over the floor (p50) |
|---|---|---|---|---|
| local `/bin/true` | 0.35 | 0.49 | 0.60 | 0 |
| rffmpeg, state on nvme | 65.78 | 67.29 | 69.05 | 65.43 |
| rffmpeg, state on tmpfs | 65.86 | 67.49 | 69.28 | 65.51 |
| rrffmpeg, SSH | 7.23 | 7.75 | 8.18 | 6.88 |
| rrffmpeg, agent | 1.56 | 1.66 | 1.75 | 1.21 |

A second run of the same rows keeps the shape: rffmpeg 66.53 ms p50, rrffmpeg
SSH 7.25 ms, rrffmpeg agent 1.56 ms.

Read the last column as the price of dispatching: 65.4 ms for rffmpeg, 6.9 ms
for rrffmpeg over SSH, 1.2 ms through the agent. rffmpeg costs 9.5 times
rrffmpeg over SSH and 54 times the agent.

## With the real ffmpeg

Same harness, `/usr/bin/ffmpeg -version` on the worker. This is what a Jellyfin
startup probe waits for.

| Row | p50 | p95 | p99 | Added over local ffmpeg (p50) |
|---|---|---|---|---|
| local ffmpeg | 24.18 | 25.30 | 25.43 | 0 |
| rffmpeg, state on nvme | 117.26 | 120.01 | 120.90 | 93.08 |
| rffmpeg, state on tmpfs | 117.51 | 128.65 | 133.91 | 93.33 |
| rrffmpeg, SSH | 33.40 | 34.72 | 37.63 | 9.22 |
| rrffmpeg, agent | 27.77 | 29.41 | 30.37 | 3.59 |

A probe that takes 24 ms locally takes 117 ms through rffmpeg and 28 ms through
the agent. Jellyfin sends several probes when a stream starts, so rffmpeg turns
100 ms of probes into 500 ms.

## Where rffmpeg spends the 65 ms

Each piece measured alone, same box:

| Cost | p50 |
|---|---|
| Python start and no import | 8.30 |
| Python start plus `click`, `yaml`, `sqlite3` | 34.51 |
| one warm SSH channel (`ssh 127.0.0.2 true`) | 4.33 |
| rffmpeg with a `localhost` host, no SSH at all | 55.22 |

So about 55 ms is the interpreter, its imports, the config parse, the SQLite
connections with their commits, and the log write. SSH adds about 10.5 ms
because rffmpeg opens two connections per job: one for its health probe,
`ssh worker ffmpeg -version`, and one for the job itself. The probe runs before
the "is the host idle" check, so an idle host pays it too.

rrffmpeg does neither: one binary starts in under a millisecond, and it writes
its journal from a background thread.

## The fsync does not show on this disk

tmpfs is not faster than nvme in the table: 65.86 ms against 65.78 ms. On this
nvme one `fdatasync` costs 0.42 ms, so four commits cost under 2 ms and the
fixed interpreter cost hides them. `docs/latency.md` has the disk numbers.

The production case is different. There the same four commits ran on a shared
disk at 43% IO pressure, and one job waited 2 to 4 seconds before ffmpeg
started. Moving the database to tmpfs brought it to about 64 ms, the interpreter
cost measured above. The code path is the defect, not this disk: the commit sits
between "decide to run" and "run". rrffmpeg appends to a journal and returns, so
neither a quiet disk nor a busy one reaches the job start.

## What this does not cover

- A real network. Everything is loopback, and both ends share one CPU, so the
  SSH rows carry no wire latency and no encryption cost worth naming.
- A real worker and a real transcode. Nothing here encodes, and the GPU is out
  of the picture. Encode throughput is unaffected by the dispatcher either way.
- Large output. `/bin/true` and `-version` print little, so a big passthrough
  such as `-encoders` is not in these rows.
- rffmpeg under load or with several hosts. One host, no queue, no concurrency.
- The one-week production criterion. This is one box, one hour.

Next:

```bash
cargo test --release --test latency -- --ignored --nocapture
```
