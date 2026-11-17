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
| Network | loopback only, no sshd on this box |

## Method

Two independent harnesses. If they disagreed, the numbers would be wrong.

1. `tests/latency.rs`, a measurement test in the crate. It starts the daemon
   in-process on port 0, times 200 probes, 200 jobs, and 200 local spawns of
   the same program. This isolates the transport: no journal, no config file,
   no slot files, and the job is `/bin/true` so no codec work is included.

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
loopback, 16 cores, 31812 MiB RAM, load 0.71 0.67 0.61
probe (one TLS connection)         n=200  p50=   0.27 ms  p95=   0.29 ms  p99=   0.31 ms  max=   0.36 ms
dispatch through the agent         n=200  p50=   0.62 ms  p95=   0.69 ms  p99=   0.73 ms  max=   0.75 ms
local /bin/true (floor)            n=200  p50=   0.29 ms  p95=   0.34 ms  p99=   0.35 ms  max=   0.37 ms
```

Read it as: a full job through two TLS connections costs 0.62 ms p50, and
starting the same program locally costs 0.29 ms p50, so the transport itself
adds **0.33 ms p50 and 0.38 ms p99**. One connection alone, the shape of a
probe, is 0.27 ms p50. That is 0.7 percent of the 50 ms budget.

## Whole dispatcher, real ffmpeg

```text
local ffmpeg            n=60  p50=27.84 ms  p95=29.90 ms  p99=31.64 ms  max=31.74 ms
rrffmpeg passthrough    n=60  p50=28.62 ms  p95=30.53 ms  p99=32.02 ms  max=32.53 ms
rrffmpeg + agent        n=60  p50=31.60 ms  p95=32.55 ms  p99=32.95 ms  max=41.86 ms
```

Nearly all of that is ffmpeg itself: `ffmpeg -version` costs 27.8 ms of CPU
before it prints anything, and no dispatcher changes that.

- The wrapper with no transport adds 0.8 ms p50 over calling ffmpeg directly.
- The agent transport adds 3.0 ms p50 over the wrapper, 0.9 ms p99.

So the whole dispatch, real ffmpeg included, is p99 32.95 ms. The dispatcher's
own share is the 3.8 ms between the first and third line.

The 3.0 ms gap is larger than the 0.33 ms the in-process harness measures. The
difference is the parts the in-process harness omits: reading the config,
opening the journal, taking a slot file, and scheduling the daemon's four
threads per job. `strace -c` on the client attributes only 0.2 ms to syscalls
on the agent path, so the remainder is CPU and thread handoff, not waiting.

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

## What these numbers do not cover

- A real network. Loopback has no RTT, no packet loss, no MTU discovery.
- SSH. There is no sshd on this machine, so the two transports are not compared
  here. The claim that the agent beats SSH per job comes from the design (one
  connection instead of one ssh process plus a probe), not from this sheet.
- A GPU worker. Nothing here transcodes.
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
