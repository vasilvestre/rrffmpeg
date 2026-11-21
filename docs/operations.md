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

## Ship the numbers to Grafana

The journal is one JSON object per line already, so no Rust code stands between
it and Grafana. Two paths, two questions.

### History: counters and histograms through Alloy

Alloy tails the journal, lifts the event fields into labels, and turns the
lines into a counter and two histograms. Counters only go up, so `rate()` and
`histogram_quantile()` work over any window, which the snapshot below cannot
do.

```hcl
loki.source.file "journal" {
  targets = [
    { __path__ = "/home/alice/.local/state/rrffmpeg/journal" },
    { __path__ = "/home/alice/.local/state/rrffmpeg/journal.*" },
  ]
  forward_to = [loki.process.journal.receiver]
}

loki.process "journal" {
  stage.json {
    expressions = {
      event    = "ev.kind",
      host     = "ev.host",
      code     = "ev.code",
      dispatch = "ev.elapsed_us",
      duration = "ev.dur_us",
    }
    // Keeps journal.seq, which is a bare number, out of Loki.
    drop_malformed = true
  }

  stage.labels {
    values = { host = "", event = "" }
  }

  stage.match {
    selector = "{event=\"started\"}"
    stage.metrics {
      metric.counter {
        name        = "rrffmpeg_jobs_total"
        description = "Jobs the dispatcher started."
        match_all   = true
        action      = "inc"
      }
      metric.histogram {
        name        = "rrffmpeg_dispatch_us"
        description = "Microseconds from process start to the worker."
        source      = "dispatch"
        buckets     = [1000, 5000, 10000, 25000, 50000, 100000, 500000]
      }
    }
  }

  stage.match {
    selector = "{event=\"finished\"}"
    stage.metrics {
      metric.counter {
        name        = "rrffmpeg_jobs_ok_total"
        description = "Jobs that ended zero."
        source      = "code"
        value       = "0"
        action      = "inc"
      }
      metric.histogram {
        name        = "rrffmpeg_job_duration_us"
        description = "Microseconds the worker spent on a job."
        source      = "duration"
        buckets     = [100000, 500000, 1000000, 5000000, 30000000, 120000000]
      }
    }
  }

  forward_to = [loki.write.local.receiver]
}

loki.write "local" {
  endpoint { url = "http://127.0.0.1:3100/loki/api/v1/push" }
}
```

Alloy serves those metrics on its own `/metrics`, with the
`loki_process_custom_` prefix, and they carry the `host` and `event` labels, so
a per-host panel works:

```promql
histogram_quantile(0.95,
  sum by (le) (rate(loki_process_custom_rrffmpeg_dispatch_us_bucket[5m])))
```

Four things to know. Set `tail_from_end = false` on the first run, or Alloy
starts after the last line and the existing journal never reaches Loki, and
nothing counts it. The counters reset when Alloy reloads its config; `rate()`
handles that. Failed jobs are `rrffmpeg_jobs_total - rrffmpeg_jobs_ok_total`,
because a counter can only match one exact value and `code` is a number. The
block label form `metric.counter "name" { }` does not parse; use
`metric.counter { name = "..." }`. This config was run against Alloy v1.11.2
on a sample journal, and those five series came out.

### Now: snapshot gauges from the textfile

`--prometheus` prints what only the whole journal can tell: percentiles over
every job, the open count, and the cap each host sized itself to, which is not
a log line. A process that exits cannot be scraped, so a timer writes the file
where Alloy's unix exporter reads it:

```bash
rrffmpeg-stats --prometheus > /var/lib/alloy/textfile/rrffmpeg.prom
```

```hcl
prometheus.exporter.unix "local" {
  textfile {
    directory = "/var/lib/alloy/textfile"
  }
}
```

The file is rewritten whole on each run, so a systemd timer is enough. The
series:

- `rrffmpeg_dispatch_seconds` and `rrffmpeg_job_duration_seconds`, each with a
  `quantile` label (`0.5`, `0.95`, `0.99`, `1`).
- `rrffmpeg_speed_ratio` and `rrffmpeg_fps`, same quantile label.
- `rrffmpeg_open_jobs`, `rrffmpeg_jobs_finished`, `rrffmpeg_reaped_jobs`.
- `rrffmpeg_host_jobs` and `rrffmpeg_host_failures`, per host.
- `rrffmpeg_jobs{kind}`, `rrffmpeg_classes{class}`, `rrffmpeg_exit_codes{code}`.
- `rrffmpeg_slots{host,class}`: the cap the next dispatch uses.
- `rrffmpeg_light_wait_over_1s` and `rrffmpeg_orphan_events{kind}`, the two
  counters that must stay at zero.

A figure with no sample is absent, not zero, so a dashboard shows a gap
instead of a measured zero. The percentiles cover the whole journal, so they
drift slowly and jump at rotation: use them for the current state, and the
Alloy counters for history.

### ELK

Filebeat reads the same lines: set `ndjson` on the input and the `ev.kind`
field becomes searchable. `--json` still prints one object with the summary
figures for anything that wants a single document. Neither path is tested here.

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
rrffmpeg-stats --prometheus > /var/lib/alloy/textfile/rrffmpeg.prom
```
