# Operations

How to read the numbers rrffmpeg already wrote, and how to turn up its log.

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

The journal also records the kind of work each job is (`probe`, `thumbnail`,
`trickplay`, `sprite`, `analysis`, `audio`, `transcode`, `remux`) and the input
path it reads, so the report splits the same figures by kind and by pool:

```text
kind      trickplay  n=14    dispatch p50 2.04 ms   p95 3.10 ms
                            duration p50 11.2 s    p95 19.4 s
kind      thumbnail  n=3     dispatch p50 746 us    p95 812 us
class     heavy 2  light 91  unknown 512
size      host uhd630  class heavy  cap 1  n 0  p05 none
parked    none
light wait  0 of 91 light jobs waited over 1 s, slowest -
```

`unknown` is a job from a journal written before the split. `size` is the cap a
host sized itself to, with the sample count and the 5th percentile behind it.
`parked` counts the time a throttled stream held a slot without using it.
`light wait` counts the Light jobs that still waited over a second, so it shows
a light pool that is too small.

What it does not measure: the delay Jellyfin sees before the first frame, and
anything on the worker beyond the process lifetime. For the week-long test, run
it from cron and keep the output, or point a log collector at the journal: it is
one JSON object per line, and `--json` gives the same figures in one object.

The benchmark behind the transport numbers, with its method and raw runs, is in
`latency.md`. The side-by-side against rffmpeg is in `compare-rffmpeg.md`.

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

Keep a record for the week-long test:

```bash
rrffmpeg-stats --json >> ~/rrffmpeg-week.jsonl
```
