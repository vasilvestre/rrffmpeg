# 0006: Per-host concurrency plus capability match, then wait, then local

Status: Superseded by ADR-0008
Date: 2026-11-17

## Context

rffmpeg's `weight` divides the process count and nothing else. It has no
concurrency cap, no idea which host supports which encoder, and it re-tests a
host with `ffmpeg -version` on every job. A single GPU worker on an N100
saturates quickly, and a transcode sent to a host without VAAPI fails after
Jellyfin has already committed to it.

Jellyfin already queues jobs on its side. A job that waits a little for the GPU
host is better than a job that runs immediately on a CPU-only client.

## Decision

Per host, in config:

- `concurrency`, default 1, is the hard cap.
- `capabilities` is an advertised list (`vaapi`, `nvenc`, `qsv`, `amf`,
  `videotoolbox`). An empty list accepts any job.

Derive the required capability from the arguments: an argument ending in one
of those names (`h264_vaapi`, `hevc_nvenc`, `h264_qsv`) requires that
capability. `-c copy` and `libx264` require none.

Selection reads in-memory atomics, never the journal:

- Skip down hosts and hosts at their cap.
- Skip hosts that do not advertise the required capability.
- Among the rest, take the lowest `running / concurrency` ratio; config order
  breaks a tie.

When no slot is free, queue in the process, lower `priority` first and FIFO on
ties. Probes are `-5`; transcodes are `0`. Wait up to `queue_wait_secs`,
default 600. If nothing frees up, log one line to stderr and run the local
binary, which is exactly what Jellyfin would have done without a dispatcher.

## Consequences

- The GPU host is never oversubscribed. Extra jobs wait instead of failing.
- A job that needs VAAPI can still fall back to a client with no GPU after the
  wait. It then fails where Jellyfin expects an error, instead of failing
  immediately with a confusing one. Set `queue_wait_secs` higher to avoid it.
- Priority only orders jobs inside one process. Jellyfin starts one process per
  job, so cross-process priority waits for the agent daemon.
- An empty capability list means "unknown, accept anything". That is the
  deliberate default for a single-worker setup, and it is the wrong setting
  once a second host exists.
- No health check runs per job. Placement trusts the janitor's flag, which is
  refreshed once per cycle.

## Alternatives rejected

- rffmpeg's `weight`. It is not a capacity model and cannot express "two 4K
  jobs is one too many".
- A per-job `ssh ffmpeg -version` probe. It cost about 90 seconds of Jellyfin
  startup and added a round trip to every dispatch.
- Probing the worker's encoders at startup. It duplicates what the config can
  state, and the config already has to know where media is mounted.
- Failing the job when no slot is free. Jellyfin handles a local failure worse
  than a slow success.
