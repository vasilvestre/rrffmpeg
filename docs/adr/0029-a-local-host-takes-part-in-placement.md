# 0029: A local host takes part in placement

Status: Accepted

Date: 2026-11-19

## Context

A client can have a usable GPU and still need a worker. The measured pair is a
client with an Intel UHD 630, which encodes H.264 and HEVC but no AV1, and an
N100 worker that does. Before this record the only way to use the client's GPU
was local mode, `no [[hosts]] at all`, which cannot be combined with a worker:
either everything runs where it lands or everything goes out.

`fallback_local` does not cover the case. It runs the local binary only when no
worker slot frees within `queue_wait_secs`, after the queue gives up, and it
checks no capability. A job that only the worker can run therefore fails, and a
job the client can run better waits for a remote slot it did not need.

ADR 0017 (one transport behind a trait) and the `Router` already dispatch by
host field. What was missing was a host kind whose transport is the local
binary.

## Decision

A `[[hosts]]` entry with `local = true` runs its jobs on the client, through
the same scheduler, journal, slot locks and queue as any other host.

- The transport is `transport::Local`: it spawns `local_ffmpeg` or
  `local_ffprobe` with `local_env` then `[hosts.env]`, pipes the tapped stream,
  relays it, writes a pid file under `state_dir/pid` before the job can be
  forgotten, and reports the exit code or 128 plus the signal.
- The child asks the kernel for `SIGTERM` when its parent dies
  (`PR_SET_PDEATHSIG`). Jellyfin cancels a transcode by killing the process it
  started, which here is the dispatcher. Local mode used to `exec`, so the
  signal reached ffmpeg; a spawned child would otherwise keep encoding into the
  same output directory and hold the GPU.
- One config takes one local host. Slots are exclusive per host name, so two
  entries would each allow their own `concurrency` on one GPU.
- `name` becomes a label. It names the journal rows, the slot locks and the
  `--host` filter; nothing connects to it.
- The config refuses what contradicts the field: `local` with `agent_addr`,
  with `paths`, with `ffmpeg_bin` or `ffprobe_bin`, or with `ssh_args`. A local
  host reads the client's own paths, so a path map would be a lie.
- `Local::new` refuses a tool that resolves to this program, whether through
  the path or through `PATH`, because a `local_ffmpeg` pointing at the ffmpeg
  symlink would fork a copy of the dispatcher per job until the box dies.
- `rrffmpeg capabilities <name>` probes a local host in place, with the same
  script and the same `--device`, and never opens ssh.
- The Jellyfin audit counts a local host as covering `TranscodingTempPath`,
  since the client writes to its own tmpfs and no map is involved.
- `probe` and `probe_capabilities` get the same environment as a job, so a
  wrapper that needs `local_env` is not marked down for missing it.

Placement is unchanged. A local host competes on capabilities, concurrency and
the proportional slot balance like every other host, so declaring it with only
what it really does is what sends the rest of the traffic elsewhere.

## Consequences

- A client that has a GPU and a worker states both, and each job goes where it
  runs best. The AV1 host is declared with `vaapi_av1` alone; the local host
  with the tokens it proved. Nothing else in the configuration changes.
- A job on a local host costs about 0.5 ms more than the same job exec'd
  directly: the fork and the journal close. Numbers and method are in
  `docs/latency.md`. With no worker configured, local mode stays 0.5 ms faster
  and still needs no state.
- The local host is probed by the janitor like any other host, so a broken
  `local_ffmpeg` marks it down instead of failing one job per call.
- A local host with an empty `capabilities` list accepts every job, the same
  rule as a worker. That is the tempting mistake: leave it empty and the client
  takes the AV1 jobs it cannot encode.
- The local binary is spawned, not exec'd, so the dispatcher process outlives
  the job by the duration of the transcode. Memory is the parent's, unchanged:
  the child does the work.

## Alternatives rejected

- **A `--local` flag on the Jellyfin audit only.** It would fix the report and
  not the routing, which is the part that was wrong.
- **A top-level `[local]` section beside `[[hosts]]`.** A second placement
  surface, with its own rules, for a host that differs by one field.
- **Deriving the local host from an empty `[[hosts]]` list.** That is local
  mode, and it cannot describe capabilities, concurrency or the queue
  participation a real placement needs.
- **An `exec`-based local host.** It saves 0.5 ms and loses the `Finished`
  event, the exit code and the slot release, so the janitor would have to
  invent what happened.
