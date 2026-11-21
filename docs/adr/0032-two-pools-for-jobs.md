# 0032 Two pools per host: Light jobs never wait behind a transcode

Status: Accepted
Date: 2026-11-21

## Context

Every Jellyfin call reaches the dispatcher, and every call took a slot from the
same pool: playback transcodes, trickplay sprite extraction, single-frame
thumbnails and `ffprobe`. On the production client the pool was one slot, so a
trickplay job queued behind a running transcode.

Measured on the production client on 2026-11-21:

- Jellyfin kills a trickplay job when no new `jpg` appears within 20 s
  (`MediaEncoder.ExtractVideoImagesOnIntervalInternal`,
  `DefaultHdrImageExtractionTimeout`). A queued job produces nothing, so it
  died: 23 errors in 25 attempts and no tile sets.
- `ffmpeg -version` through the dispatcher on an idle box took 60 s and printed
  `no worker slot after 60s, running locally` while one trickplay job held the
  only slot. The same call against `local_ffmpeg` takes 4 ms.
- Two concurrent calls at 01:51 both printed that line with `concurrency = 2`,
  because one transcode and one trickplay held both slots.

Raising `concurrency` does not fix it. The first transcode and the first
trickplay still fill the pool, and two transcodes on one iGPU encode slower
than one. Probes already get class `-5` in the queue, which only decides who
goes first among waiters: they still wait.

The second half of the defect is the local transport. Before `local = true`
existed, the dispatcher replaced itself with ffmpeg through `exec`, so killing
the wrapper killed the worker. A schedulable local host spawns a child instead,
and the only thing tying that child to the wrapper is a death signal.

## Decision

1. Classify each job from its program and arguments, in one function
   (`Class::of`). `Light` is an `ffprobe`, an `-f image2` output, `-frames:v 1`,
   `-skip_frame nokey`, or MJPEG sprite extraction. Everything else is `Heavy`.
   The rule is a list of the shapes Jellyfin actually emits, not a general
   argument parser.
2. Give every host a second pool, `light_concurrency`, default 2. The two pools
   use separate lock files (`light.<n>` next to `slot.<n>`) and separate queue
   lanes, so a Light job never ranks against a transcode and never takes its
   slot. `concurrency` keeps its meaning and its default.
3. A Light job never waits `queue_wait_secs`. It takes a free light slot at
   once. When its own pool is full on a `local = true` host, it runs on the
   local binary without a slot, because a few frames of work cost less than the
   wait and Jellyfin kills it after 20 s. When the only hosts are remote, it
   gives a remote light slot 5 s (`LIGHT_WAIT`), then falls back like any other
   job.
4. The local child gets `PR_SET_PDEATHSIG` with `SIGKILL`, in its own session.
   `SIGTERM` is not enough: ffmpeg writes its trailer before it exits, which is
   exactly the child an operator finds still running after `kill -9`. The
   session keeps the child out of the terminal's process group, so a Ctrl-C
   stops the wrapper and lets one death signal stop the child.
5. The janitor clears the slot file of any process that is gone, on every host
   including the local one. The kernel already drops the `flock` when a holder
   dies, so this is not what makes the slot available again: it keeps the pid
   written in the file from naming a live stranger after the kernel reuses it,
   which is what a reader of `coord/*/slot.*` checks.
6. The `started` event records `class` and `pool`, so `rrffmpeg-stats` prints a
   class split and a journal reader can tell an extraction from a transcode.

## Consequences

- Two lock namespaces and two queue lanes per host, so `coord/<host>/` holds
  `slot.N` and `light.N`, and `coord/<host>/queue/` sits beside
  `coord/<host>/light/`. A reader of the old paths still finds the heavy slots.
- A host now runs up to `concurrency + light_concurrency` jobs. On one iGPU the
  light ones are video decodes too, so a burst of trickplay scans can slow a
  transcode. That is accepted: the alternative, which the production box
  measured, is that the scans never run at all.
- Light overflow on a local host breaks the cap it just measured: the third
  concurrent extraction on a full light pool runs uncapped. The alternative is
  a job that produces nothing and is killed by Jellyfin.
- `light_concurrency` is a new config key, so it is a minor version. A config
  without it gets 2.
- `rrffmpeg-stats` gains one line and one JSON key (`class`). Old journals
  replay: both new fields are defaulted and an empty class counts as `unknown`.
- The janitor's slot reap is advisory. It cannot free a lock whose holder is
  alive and must not touch one whose recorded pid is alive.

## Alternatives rejected

- **Raise `concurrency`.** Measured: at 2, a transcode and a trickplay still
  filled the pool, and a second transcode queues behind both. It also asks one
  iGPU for two encodes.
- **Priority only.** Probes already run at class `-5` and still wait. Priority
  orders waiters; it does not create capacity.
- **An in-memory pool.** Jellyfin forks one process per job, so a counter in
  memory caps nothing. ADR 0008 chose the lock file for the same reason.
- **A per-job weight or CPU accounting.** The job is a black box of arguments;
  the split that matters is "reads a few frames" against "encodes a stream",
  and the arguments already say which.
- **Block the child with `prctl(PR_SET_PDEATHSIG, SIGKILL)` only.** A process
  that ignores `SIGTERM`, which ffmpeg does while it writes a trailer, survives
  the wrapper. The death signal has to be one the child cannot defer.
