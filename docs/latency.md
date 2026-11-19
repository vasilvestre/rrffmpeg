# Measured dispatch latency

Every number here was produced on the machine below, on loopback, and every
command is in this file so you can repeat it. Measurements are milliseconds.
Percentiles are nearest rank.

What this answers: how much the dispatcher adds to a transcode start. The
project budget is a dispatch p99 under 50 ms.

## Machine

| | |
|---|---|
| CPU | 11th Gen Intel Core i7-11800H, 2.30 GHz, 16 threads |
| RAM | 31 812 MiB |
| Kernel | 7.2.15-76070215-generic |
| Load during the runs | 0.6 to 0.7 on 16 threads |
| Network | loopback only, with sshd on the same box for the SSH rows |

## Method

Two independent harnesses. If they disagreed, the numbers would be wrong.

1. `tests/latency.rs`, a measurement test in the crate. It starts the daemon
   in-process on port 0, times 200 probes, 200 jobs, and 200 local spawns of
   the same program. This isolates the transport: no journal, no config file,
   no slot files, and the job is `/bin/true` so no codec work is included. The
   `dispatch_latency_ssh` test in the same file does the same against an sshd
   on this box, through a `Host` with no `agent_addr`, which is what selects
   the SSH transport.

```bash
cargo test --release --test latency -- --ignored --nocapture
```

2. A shell loop with the real release binaries and the real ffmpeg 8.0.1 from
   Ubuntu. This measures the whole dispatcher: config parse, journal, slot
   files, TLS, and the daemon. Timestamps come from `date +%s%N` around each
   run, 5 warmup runs are dropped, then 60 samples are kept.

```bash
cd /tmp/lat
printf "local ffmpeg          "; ./measure.sh 60 /usr/bin/ffmpeg -version
printf "rrffmpeg passthrough  "; RRFFMPEG_CONFIG=$PWD/local.toml  ./measure.sh 60 ./bin/ffmpeg -version
printf "rrffmpeg + agent      "; RRFFMPEG_CONFIG=$PWD/config.toml ./measure.sh 60 ./bin/ffmpeg -version
```

## Transport cost

Release build, daemon in-process, job is `/bin/true`.

```text
loopback, 16 cores, 31812 MiB RAM, load 1.20 0.93 0.67
probe (one TLS connection)         n=200  p50=   0.27 ms  p95=   0.36 ms  p99=   0.40 ms  max=   0.43 ms
dispatch through the agent         n=200  p50=   0.64 ms  p95=   0.72 ms  p99=   0.75 ms  max=   0.80 ms
local /bin/true (floor)            n=200  p50=   0.30 ms  p95=   0.34 ms  p99=   0.37 ms  max=   0.46 ms
```

Read it as: a full job through the agent costs 0.64 ms p50, and starting the
same program locally costs 0.30 ms p50, so the agent transport adds **0.34 ms
p50 and 0.38 ms p99**. One connection alone, the shape of a probe, is 0.27 ms
p50 and that connection is now the only one a silent job opens (ADR 0018). That
is 0.7 percent of the 50 ms budget.

Where the 0.27 ms of one connection goes, measured by timing a bare TCP
connect and close to the same daemon against a full probe, 100 runs each:

```text
tcp connect+close                  n=100  p50=  19 us  p95=  23 us  p99=  37 us
probe (connect+handshake+status)   n=100  p50= 263 us  p95= 387 us  p99= 576 us
```

So the TCP connect is 7 percent of a probe. Almost all of it is TLS: the
handshake, the daemon's thread for the connection, and the status round trip.
Instruction counts from `valgrind --tool=callgrind` on one whole dispatch agree
that the crypto dominates the client's CPU (4.4 M instructions, of which
X25519 19 percent, P-256 30 percent, SHA-512 14 percent), which is why a
cheaper signature scheme was tried and rejected: an Ed25519 certificate in
place of the P-256 one moved nothing, because that CPU is not what the client
waits for. Round trips are.

The same job over SSH:

```text
loopback, 16 cores, 31812 MiB RAM, load 0.45 0.74 0.83
probe (one ssh connection)         n=200  p50= 108.31 ms  p95= 121.70 ms  p99= 127.09 ms  max= 128.67 ms
dispatch through ssh               n=200  p50= 112.99 ms  p95= 126.92 ms  p99= 130.68 ms  max= 143.41 ms
local /bin/true (floor)            n=200  p50=   0.26 ms  p95=   0.35 ms  p99=   0.42 ms  max=   0.47 ms
```

Against the 0.26 ms floor, SSH adds **112.73 ms p50 and 130.26 ms p99**, more
than twice the 50 ms budget before ffmpeg starts. Almost none of that is this
crate: `ssh -o BatchMode=yes localhost true` takes 112 ms of wall clock with
10 ms of client CPU, and `strace` puts the time in waits on the ssh socket. It
is sshd session setup on this machine, not the dispatcher and not the wire. The
round trip section separates the wire part from this constant.

The two transports pay different shapes. SSH spawns one `ssh` process and opens
one connection per job, and inside that connection the TCP connect, version
exchange, key exchange, authentication, command start and exit cost about 15
one-way trips. Each 10 ms of one-way delay adds 165 ms to a job, so the wire
part is about 8 round trips. The agent spawns no
client process for a job. It opens one TLS connection to a daemon that already
runs, and a second one only when the job writes `stdin` (ADR 0018). A probe
costs 3 round trips, the same as a job.

## SSH connection persistence

The SSH rows above spawn one `ssh` per job. `ssh_persist_secs` keeps one
connection per host open instead, so a job opens a channel on it. Same shell
harness, real binaries, N=30 with `ssh_persist_secs = 0` and N=60 with the
default 300, against an sshd on `127.0.0.2`:

```text
SSH, persistence off   p50 155.60 ms  p99 173.55 ms
SSH, persistence 300   p50  37.94 ms  p99  46.94 ms
agent                  p50  30.95 ms  p99  33.92 ms
```

On a LAN, SSH with persistence lands 7 ms from the agent and inside the 50 ms
budget. The sockets live on tmpfs, `$XDG_RUNTIME_DIR/rffmpeg` or
`/dev/shm/rrffmpeg-<uid>`, mode 0700. The location does not change the
latency: 37.96 ms p50 from the runtime directory against 38.00 ms from an ext4
path, both N=60. It buys hygiene, since a reboot empties the directory. A stale
socket is a fallback, not a failure: `ControlMaster=auto` opens a new
connection when the master is gone. ADR 0014 records the choice.

## Network round trip

Loopback has no latency, so this section adds some. The agent rows use `netem`
on the loopback device, inside an unprivileged network namespace. `netem`
delays each packet in both directions, so a 10 ms setting is about 20 ms of
round trip per connection. The setting is not the round trip.

The harness is the same `dispatch_latency` test, N=200, run inside that
namespace. No root is needed:

```bash
cargo test --release --test latency --no-run
BIN=$(ls -t target/release/deps/latency-* | grep -v '\.d$' | head -1)
for D in 1 10 30; do
  unshare -rn sh -c "ip link set lo up && tc qdisc add dev lo root netem delay ${D}ms && $BIN --ignored --nocapture --test-threads=1 dispatch_latency"
done
```

The SSH rows cannot use that namespace. A new network namespace has its own
loopback with no sshd, and sshd will not start in an unprivileged one on this
box: its session setup calls `setgroups`, and the namespace denies it. They
put a TCP relay in front of the real sshd instead. It listens on 2222, connects
to 22, and waits the same delay on each chunk in each direction, so a 10 ms
setting is again about 20 ms of round trip. `tests/relay.py` takes the one-way
delay in milliseconds. The dispatcher calls `ssh` with the host name only, so
`tests/shim/ssh`, a two line wrapper on `PATH`, adds `-p 2222`:

The first run needs the relay's host key, which is sshd's, filed under the
relay port: `ssh-keyscan -p 2222 localhost >> ~/.ssh/known_hosts`.

```bash
cargo test --release --test latency --no-run
BIN=$(ls -t target/release/deps/latency-* | grep -v '\.d$' | head -1)
for D in 0 1 10 30; do
  python3 tests/relay.py $D &
  PATH=$PWD/tests/shim:$PATH $BIN --ignored --nocapture --test-threads=1 dispatch_latency_ssh
  kill %1
done
```

With the delay at zero the relay matches the direct path, 108.12 ms p50 against
108.31 ms p50, so the relay costs nothing worth naming. Both ends are localhost,
so the delay is the only network cost in the SSH rows.

1 ms of delay on each direction:

```text
agent run, load 0.62 0.90 0.83
probe (one TLS connection)         n=200  p50=   6.46 ms  p95=   6.62 ms  p99=   6.73 ms  max=   7.80 ms
dispatch through the agent         n=200  p50=   6.74 ms  p95=   6.87 ms  p99=   6.94 ms  max=   7.24 ms
local /bin/true (floor)            n=200  p50=   0.26 ms  p95=   0.36 ms  p99=   0.40 ms  max=   0.42 ms
ssh run, load 1.36 1.04 0.92
probe (one ssh connection)         n=200  p50= 122.30 ms  p95= 129.69 ms  p99= 134.03 ms  max= 139.99 ms
dispatch through ssh               n=200  p50= 125.06 ms  p95= 132.32 ms  p99= 134.61 ms  max= 135.77 ms
local /bin/true (floor)            n=200  p50=   0.28 ms  p95=   0.37 ms  p99=   0.41 ms  max=   0.44 ms
```

10 ms:

```text
agent run, load 0.62 0.90 0.83
probe (one TLS connection)         n=200  p50=  60.99 ms  p95=  61.45 ms  p99=  61.58 ms  max=  61.83 ms
dispatch through the agent         n=200  p50=  61.70 ms  p95=  62.06 ms  p99=  62.21 ms  max=  62.34 ms
local /bin/true (floor)            n=200  p50=   0.27 ms  p95=   0.40 ms  p99=   0.53 ms  max=   0.61 ms
ssh run, load 1.41 1.10 0.95
probe (one ssh connection)         n=200  p50= 278.12 ms  p95= 285.24 ms  p99= 289.29 ms  max= 301.09 ms
dispatch through ssh               n=200  p50= 278.23 ms  p95= 285.04 ms  p99= 293.49 ms  max= 300.62 ms
local /bin/true (floor)            n=200  p50=   0.27 ms  p95=   0.38 ms  p99=   0.46 ms  max=   0.60 ms
```

30 ms:

```text
agent run, load 0.41 0.82 0.80
probe (one TLS connection)         n=200  p50= 181.07 ms  p95= 181.48 ms  p99= 181.66 ms  max= 182.58 ms
dispatch through the agent         n=200  p50= 181.71 ms  p95= 182.07 ms  p99= 182.31 ms  max= 182.51 ms
local /bin/true (floor)            n=200  p50=   0.26 ms  p95=   0.38 ms  p99=   0.46 ms  max=   0.60 ms
ssh run, load 0.75 0.98 0.92
probe (one ssh connection)         n=200  p50= 565.43 ms  p95= 595.55 ms  p99= 597.61 ms  max= 598.10 ms
dispatch through ssh               n=200  p50= 565.08 ms  p95= 596.16 ms  p99= 598.98 ms  max= 613.41 ms
local /bin/true (floor)            n=200  p50=   0.27 ms  p95=   0.38 ms  p99=   0.43 ms  max=   0.62 ms
```

Read the agent rows as: a probe costs three round trips, so a 10 ms setting
(20 ms round trip) puts a probe at about 61 ms. Dispatch tracks the probe, and
the local floor does not move, which confirms the cost is the wire, not the
machine.

Read the SSH rows against the same floor, about 0.27 ms, so the delta is the
whole transport: 124.78 ms at 1 ms, 277.96 ms at 10 ms and 564.81 ms at 30 ms.
The agent's deltas are 6.48, 61.43 and 181.45 ms. SSH is 19 times the agent at
1 ms, 4.5 times at 10 ms and 3.1 times at 30 ms.

That ratio falls because SSH carries a fixed cost that the delay does not
touch. At zero RTT it already pays about 108 ms of sshd session setup on this
box, and delay adds about 15 one-way trips on top of it, which is 7 to 8 round
trips. A job costs the agent 3 round trips.

At 30 ms the SSH dispatch p99 is 598.98 ms, twelve times the 50 ms budget. The
agent's 182.31 ms is already more than three times it.

The rows also bound the link. The whole dispatcher costs 31.28 ms p50 with real
ffmpeg at zero RTT, and an agent probe is three round trips, so an RTT above
about 6 ms spends the rest of the budget before ffmpeg starts. Keep the client
and the worker on the same LAN.

No change in this repository removes that: the client waits for the network.
The transport choice does change it. At 10 ms of delay an agent dispatch is
62.21 ms p99 and an SSH dispatch is 293.49 ms p99. Use the agent on any link
with delay, and keep SSH for a LAN where no daemon runs.

The relayed byte counts are not in these runs because `/bin/true` prints
nothing. Set `RRFFMPEG_LOG=debug` and the client prints
`job <id> relayed <n> B stdout, <m> B stderr in <t> ms` per job, which is how
you see the cost of a large passthrough such as `ffmpeg -encoders`. The SSH
transport hands inherited file descriptors to `ssh`, so it cannot count bytes.
Only the agent path logs them. No number has been taken for a large payload yet.

## When the worker is oversubscribed

One client at a time hides the queue. Jellyfin opens several jobs at once
during a scan, and a worker with `concurrency = 8` then has more callers than
slots. 16 client processes in parallel, 320 jobs each run, agent on loopback,
job is `/bin/true` so the number is the queue and the transport only.

| Run | p50 | p95 | p99 | max |
|---|---|---|---|---|
| before ADR 0017 | 2.60 ms | 3.82 ms | 42.90 ms | 43.77 ms |
| after ADR 0017 | 4.21 ms | 4.67 ms | 6.45 ms | 12.66 ms |
| before ADR 0017 | 2.50 ms | 3.78 ms | 292.67 ms | 314.66 ms |
| after ADR 0017 | 4.30 ms | 4.66 ms | 6.33 ms | 12.53 ms |
| before ADR 0017 | 2.50 ms | 3.68 ms | 130.78 ms | 303.95 ms |
| after ADR 0017 | 4.28 ms | 6.38 ms | 12.40 ms | 31.36 ms |

Every pair is one run before and one run after, back to back, same machine,
same load.

The tail is the number that matters: before, a waiter lost its turn to every
newcomer and drained behind a doubling backoff, so one queue of 16 reached
100 ms and once 1 s. After, a queued job passes as soon as a slot is free and
no better ticket waits, so the tail is two or three poll intervals.

The p50 rises because the order is now real: with twice as many clients as
slots, half of them wait for a slot instead of taking one out of turn.

`scripts/measure.py` runs one command at a time, so this table comes from a
small parallel harness: `w` threads each running the same command, the elapsed
time of every `subprocess.run` kept. It lives in `/tmp`, not in the repository,
because it measures a property the crate tests already cover functionally
(`a_newcomer_does_not_take_the_slot_a_waiter_queued_for`).

## Whole dispatcher, real ffmpeg

```text
ffmpeg direct           n=60  p50=25.32 ms  p95=26.52 ms  p99=28.51 ms  max=28.51 ms
rrffmpeg + agent        n=60  p50=27.19 ms  p95=28.37 ms  p99=28.91 ms  max=28.91 ms
```

Nearly all of that is ffmpeg itself: `ffmpeg -version` costs 25 ms of CPU before
it prints anything, and no dispatcher changes that. The dispatcher adds **1.87
ms p50 and 0.40 ms p99** to a real ffmpeg run, which is the same 1.65 ms it
adds to `/bin/true`: the transport and the bookkeeping around it, and nothing
that grows with the job.

The rest of the client's share is bookkeeping, measured with the `hotpath`
feature on one dispatch: config 40 us, journal open 56 us, slot 38 us, agent
build 20 us, journal append 4 us. A whole `elapsed_us`, from process start to
the job going out, is 163 us p50 and 198 us p99 on this box. Process start
dominates that, and it is where a statically linked binary wins: a trivial
dynamically linked program starts in 340 us here against 240 us static, so the
release binaries, which are `musl` static, start 100 us faster than these `gnu`
measurements show.

## A job on a local host

A `[[hosts]]` entry with `local = true` runs the job on the client itself. It is
the only transport with no network, so what is left is the dispatcher's own
work. `benches/local.rs` measures it, with `/bin/true` in place of ffmpeg:

```bash
cargo bench --bench local
```

```text
spawn_true               632 us   one bare fork, exec and wait: the floor
local_spawn              636 us   one job through the local transport
journal_open_close        50 us   what a one-shot process pays to record it
journal_open_write_close  57 us   the same, with the Started and Finished events
```

The local transport costs **4 us over a bare spawn**: two pipes, one relay
thread, one pid file. It has nothing left to optimize.

The phases of a whole dispatch come from a temporary instrumented build of the
same revision, three runs each, load 1.8 on 16 threads:

```text
config load                      105 us
journal open + janitor spawn     135 us
slot acquire                      30 us
journal append (Started)           9 us
spawn, pid file, relay setup     200 us
wait for the child            30 600 us   ffmpeg -version, unchanged
journal close after the job       50 us
```

So a job on a local host costs about **0.5 ms more than the same job exec'd
directly**, which is under 2 percent of a 30 ms ffmpeg start. The one structural
cost is the fork: local mode replaces the process with `exec`, while a local
host must fork, wait and record, because it owes a `Finished` event to the
journal.

A wall-clock comparison of the two modes on this box is not usable: the paired
delta over 150 runs moves between -4 ms and +6 ms, because process start and
scheduler placement on a shared runner move more than the effect being
measured. The per-phase numbers above are the ones to trust, and they say the
local path is already at the floor.

## Journal growth

The client opens the journal and replays it on every dispatch, so a long
running client reads a longer file each time. Same harness, only the state
directory differs.

```text
journal fresh, empty     n=40  p50=31.28 ms  p95=33.49 ms  p99=35.40 ms  max=39.73 ms
journal 3000 lines       n=40  p50=32.61 ms  p95=34.40 ms  p99=34.51 ms  max=42.16 ms
```

3000 records, 216 KiB, cost about 1.3 ms p50. Rotation caps the file at 8 MiB,
so the worst case is bounded, but a replay this expensive per job is worth
watching: it is bookkeeping on the critical path, which is exactly what ADR
0002 set out to remove. Moving the replay into the background would need a
different way to answer `active_on`, so it is not done.

## Journal replay at the rotation cap

`Journal::open` replays the whole file, so the file size is a per-job cost.
Rotation caps the file at `ROTATE_BYTES`, 8 MiB in `src/state.rs`. The harness
fills a journal to the cap, 82 730 records and 8 388 648 B, and times the open
five times:

```text
journal_replay_at_rotation_cap: 82730 records, 8388648 B, cap 8388608 B
run 0:   36.820 ms
run 1:   35.066 ms
run 2:   34.732 ms
run 3:   34.031 ms
run 4:   33.821 ms
median 34.732 ms, 0.420 us per record
```

At the cap a replay costs 34.732 ms median, and it sits on the critical path,
between deciding to run and running ffmpeg. That is most of the 50 ms budget for
bookkeeping alone. The five runs fall as the page cache warms, from 36.820 ms
to 33.821 ms, and settle at 0.420 us per record. That is most of a 50 ms
budget for bookkeeping alone. ADR 0015 takes it off the critical path; the
numbers below are the before and after.


## Journal replay moved off the dispatch path

The first version replayed the whole journal inside `Journal::open`, before the
job was placed. Each file is capped at 8 MB and nothing caps the number of
files, so the cost grew with everything on disk. ADR 0015 moved the replay to
the janitor's first read. Same machine, 60 jobs per size, `elapsed_us` read
back from the journal, transport `/bin/true`:

| Journal on disk | Before p50 | Before p99 | After p50 | After p99 |
|---|---|---|---|---|
| empty | 180 us | 208 us | 130 us | 183 us |
| 1 MB active file | 4.3 ms | 5.3 ms | 134 us | 192 us |
| 4 MB active file | 16.3 ms | 18.6 ms | 134 us | 203 us |
| 8 MB active file | 32.6 ms | 34.0 ms | 133 us | 196 us |
| 32 MB in four rotated files | 113.1 ms | 117.8 ms | 132 us | 265 us |

A separate run shows the cost the janitor probe put on exit. A fake `ssh` that
sleeps 3 s on the probe and returns at once for the job: the job's wall clock
was 3.03 s before, 0.00 s after.

```bash
cargo build --release --features hotpath
HOTPATH_OUTPUT_PATH=/tmp/report.txt RRFFMPEG_CONFIG=./config.toml \
  ./target/release/rrffmpeg -version
cat /tmp/report.txt
```

With an 8 MB active file, `state::open` was 62.3 ms of a 62.9 ms run before,
and 45 us of a 0.68 ms run after. The report adds its own overhead, so compare
those two rows to each other, not to the table above.

## State directory: disk against tmpfs

The advice used to be "put `state_dir` on tmpfs". The measure says it is not
needed. The journal append is async: the flusher writes and syncs at most every
500 ms, and once more when the process exits. Same box, loopback, agent
transport, release binaries, load 1.30 to 1.68 on 16 threads.

Dispatcher only, `ffmpeg_bin = "/bin/true"` on the worker, fresh journal, n=200:

```text
nvme     p50=   2.34 ms  p95=   2.71 ms  p99=   3.75 ms  max=  11.67 ms
tmpfs    p50=   1.80 ms  p95=   1.91 ms  p99=   1.97 ms  max=   2.51 ms
```

Real ffmpeg, `ffmpeg -version` through the agent, n=60:

```text
plain ffmpeg -version        p50=  26.37 ms  p95=  29.99 ms  p99=  38.58 ms
nvme  state_dir              p50=  28.84 ms  p95=  29.93 ms  p99=  31.37 ms
tmpfs state_dir              p50=  27.72 ms  p95=  29.35 ms  p99=  30.55 ms
```

The same two runs with the journal filled to the 8 MiB rotation cap, 85 070
records, dispatcher only, n=40:

```text
nvme     p50=  53.87 ms  p95=  60.76 ms  p99=  77.12 ms
tmpfs    p50=  51.62 ms  p95=  56.40 ms  p99=  61.99 ms
```

The two raw costs behind those rows, measured alone:

```text
append 100 B and fdatasync, n=200
nvme     p50=   0.420 ms  p99=   0.811 ms  max=   5.575 ms
tmpfs    p50=   0.001 ms  p99=   0.001 ms  max=   0.009 ms

read the 8 MiB journal with O_DIRECT, page cache cold
nvme     8.2 ms
tmpfs    2.2 ms
```

Read it as: the filesystem costs 0.54 ms p50 on a fresh journal and 2.25 ms at
the cap. The replay costs about 50 ms on both. So the disk is not what the
budget is spent on, and tmpfs does not buy back the replay. (Those 8 MiB rows
predate the 2026-11-19 fix below: dispatch no longer replays at all.)

Two limits stay. A shared or spinning disk is not in these rows: one
`fdatasync` there costs tens of milliseconds, and that is the case where tmpfs
still pays. If the kernel evicts the 8 MiB between two dispatches, a cold read
adds 8 ms on nvme.

To repeat: point two configs at an nvme directory and at `/dev/shm`, run
`rrffmpeg-agent` on `127.0.0.1:9876`, and time a few hundred
`rrffmpeg -version` calls with `$EPOCHREALTIME` around each one. Set
`ffmpeg_bin = "/bin/true"` in `[[hosts]]` to keep the codec work out, and leave
it out for the real ffmpeg rows.


## The replay came back on the dispatch path, then left again

Between ADR 0015 and 2026-11-19 the `started` event read `in_flight` from
`Journal::active_on`, which ran the full replay on every dispatch. The janitor
still slept through short jobs, so a process paid the replay and never used it.
`in_flight` now comes from the slot locks the lease already holds, and dispatch
never calls `active_on`.

Same box, `ssh_bin = "/bin/true"`, synthetic `finished` records, state
directory on `/dev/shm`, n=200 for the empty journal and n=20 for the filled
one:

| Journal on disk | Dispatch, before | Dispatch, after |
|---|---|---|
| empty | 2.1 ms | 1.8 ms |
| 6.7 MB | 35.8 ms | 1.95 ms |

A journal at the rotation cap now costs a dispatch nothing. The janitor still
pays the one-time replay in a process that lives past one interval, off the
dispatch path.


## Resources per job

500 jobs through the in-process agent daemon, 16 cores. The daemon runs inside
the test process, so these counts include the harness. Only the delta matters.

```text
before  RSS     5776 KiB  threads    4  fds    7
after   RSS     6016 KiB  threads    4  fds    7
per job dRSS +0.48 KiB  dthreads +0.00  dfds +0.00
```

Threads and file descriptors stay flat, so the daemon keeps nothing per job.
Counted from outside, on the real daemon: `/proc/<agent>/task` holds 1 entry
when idle, 2 one second into a 3 s job (ADR 0018), where the relay before it
needed 4.
RSS grows by half a KiB per job, which is bookkeeping growth, not a thread or a
descriptor. The per-job cost of the daemon is negligible.

## Janitor recovery

The janitor probes a down host once per interval and then sleeps, so recovery
time is bounded by the interval, not by the probe. The production interval is
10 s, from `src/main.rs`.

```text
janitor_recovery: interval 10 s
run 0: down to up     5.11 ms, fault cleared to recovered  9999.22 ms
run 1: down to up     5.12 ms, fault cleared to recovered  9997.32 ms
run 2: down to up     5.11 ms, fault cleared to recovered  9996.52 ms
```

A host that is already up when the janitor spawns is up in about 5 ms, one
probe. A host whose fault clears between two probes waits for the next probe,
so it comes back in one interval, here 9996 to 9999 ms. Lower the interval to
recover faster. The probe itself is not the limit.

## What these numbers do not cover

- A real network. Loopback has no RTT, no loss, no reordering. The `netem` runs
  add delay only; a real link also drops and reorders packets, and `netem` is
  emulation, not a link.
- SSH, partly. The SSH rows run against an sshd on the same box, so both ends
  share one CPU and there is no encryption cost worth naming. That sshd is also
  slow at session setup: `ssh localhost true` takes 108 ms, 10 ms of it client
  CPU. A second sshd started as the user with `UsePAM=no` answers in 70 ms, so
  about 40 ms of the 108 is PAM and login bookkeeping. A worker whose sshd does
  not share a busy desktop does better at zero
  RTT. The delayed rows still show the shape, because the delay part is round
  trips, and SSH pays about 16 per job against the agent's 3.
- A GPU worker and a full transcode. Nothing here transcodes; the job is
  `/bin/true`.
- Large payloads. `/bin/true` prints nothing, so the byte-count log is the only
  handle on a big passthrough, and no number was taken for it.
- The production success criterion, p99 under 50 ms for a week. This is one
  box, one hour. The in-flight measure for that is `rrffmpeg-stats`, which reads
  the journal the dispatcher already writes: `dispatch` is microseconds from
  process start to the job going out, `duration` is the gap between the
  `started` and `finished` events. It stops at the transport, so it is a lower
  bound on what a client feels.

Next:

```bash
cargo test --release --test latency -- --ignored --nocapture
```
