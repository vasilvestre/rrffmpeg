# rffmpeg against rrffmpeg, measured

How much does each dispatcher add between "Jellyfin calls ffmpeg" and "ffmpeg
starts"? The project pitch says rffmpeg waits on a database commit and probes
the worker on every job. This page puts both programs through the same harness,
twice: on one controlled box, then on the production pair this project
replaces.

Both settings were measured on 2026-11-17. Measurements are milliseconds.
Percentiles are nearest rank.

## In this page

- **Test box**: one loopback machine, both programs, 60 samples, low load. The
  controlled numbers.
- **Production pair**: the real Jellyfin host and its GPU worker, 20 rounds
  interleaved, load 10. The numbers that matter. Jump to
  [On the production pair](#on-the-production-pair).

## What is compared

| Program | Version | Transport |
|---|---|---|
| rffmpeg | `f54ac84`, 2025-11-03 | SSH, one connection per job with persistence |
| rrffmpeg | 0.3.0 | SSH, `ssh_persist_secs = 300` |
| rrffmpeg | 0.3.0 | `rrffmpeg-agent` on `127.0.0.1:9876` |

On the test box all four send the job to `127.0.0.2`, an address served by the
sshd on the same box. rffmpeg runs its own `ffmpeg` symlink from a checkout,
rrffmpeg runs the release binary. Both reach `/usr/bin/ffmpeg` 8.0.1 on the
worker side.

The job is `-version`, first with the real ffmpeg, then with `/bin/true` in
place of ffmpeg to remove codec work and keep only the dispatch.

## Test box

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

The production section uses the `--pair` mode of the same script: two commands,
run one after the other, so a slow drift in the host hits both equally. It
drops 3 warmup rounds and keeps 20.

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

## On the production pair

The deployment this project replaces moved from rffmpeg to rrffmpeg on
2026-11-17. Both programs were still on the client, so the comparison can run
there: same Jellyfin host, same worker, same SSH key, same worker shim.

| | client | worker |
|---|---|---|
| Role | Jellyfin, dispatcher | GPU, VAAPI, `tonemap_opencl` |
| CPU | 16 threads, shared host | Intel N100 |
| Load during the runs | 9 to 13 on 16 threads | idle |
| State | `/dev/shm` (tmpfs) or the shared ext4 root at 87% full | none |
| Worker ffmpeg | | jellyfin-ffmpeg 7.1.4 |

Commands: `ffmpeg -version` through each dispatcher, 20 rounds interleaved so
drift hits both equally, 3 warmup rounds dropped, SSH control master open.

```bash
python3 scripts/measure.py --pair 20 \
  "RFFMPEG_CONFIG=~/rffmpeg.yml ffmpeg -version" \
  "RRFFMPEG_CONFIG=~/config.toml rrffmpeg -version"
```

| Job | p50 | p90 | p99 |
|---|---|---|---|
| rffmpeg, state on tmpfs | 206.4 | 234.3 | 258.5 |
| rrffmpeg, old worker shim | 52.4 | 69.7 | 143.6 |
| rrffmpeg, new worker shim | 53.8 | 78.3 | 101.4 |

The local `jellyfin-ffmpeg -version` on the client takes 30.1 ms p50, so the
dispatcher adds 176 ms for rffmpeg and 23 ms for rrffmpeg.

Both worker shims run the same `jellyfin-ffmpeg`, and the old one differs only
by the path loop it runs first. The two rrffmpeg rows show it costs nothing
measurable.

Dispatch alone, worker ffmpeg replaced by `/bin/true`, two passes of 25 rounds:

| Row | pass 1 | pass 2 |
|---|---|---|
| rffmpeg, state on tmpfs | 155.8 | 164.3 |
| rrffmpeg, state on tmpfs | 32.8 | 34.1 |
| rrffmpeg, state on the shared ext4 | 71.7 | 73.6 |

Three things to read here.

- rffmpeg costs 4.8 times rrffmpeg before any ffmpeg runs. The client is a
  shared seedbox at load 10, so Python startup and imports, measured at 34.5 ms
  on an idle laptop, cost far more here, and rffmpeg opens two SSH sessions per
  job: one for its health probe, one for the job.
- rffmpeg runs the worker ffmpeg twice, once for the probe and once for the
  job. rrffmpeg runs it once. That is the extra gap in the table above: 154 ms
  in the real row against 123 ms in the dispatch row, with one ffmpeg start
  costing about 25 ms on this worker.
- The journal filesystem still matters. On the client's own disk, 87% full and
  shared with everything else, rrffmpeg is 39 ms slower per dispatch than on
  tmpfs. That is the 2 to 4 s production defect in miniature, and it is why the
  production config puts `state_dir` on `/dev/shm`.

The absolute values move between passes. A wider pass with eight rows on the
same box gave 342.8 and 366.1 for rffmpeg, and 116.0 to 143.6 for rrffmpeg,
with the load no lower. The ratio did not move: 2.9 to 3.9 times in favour of
rrffmpeg, and 4.8 times on the dispatch alone.

## What this does not cover

- A transcode. Every row is `-version` or `/bin/true`. Nothing encodes, so this
  page says nothing about GPU throughput.
- Large output. `/bin/true` and `-version` print little, so a big passthrough
  such as `-encoders` is not in these rows.
- One host, no queue, no concurrency. rffmpeg with several workers, or a full
  slot, is not measured.
- A quiet production box. The Jellyfin host is a shared machine at load 10, and
  other tenants move the absolute numbers by up to a factor of 1.8 between
  passes. The test box rows carry loopback only, so they lose the wire latency
  and show no queueing.
- The one-week production criterion. This is one hour on each setting.

Next:

```bash
cargo test --release --test latency -- --ignored --nocapture
```
