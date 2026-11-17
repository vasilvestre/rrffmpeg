# 0013: Retry a job that never started on another host

Status: Accepted
Date: 2026-11-17

## Context

ADR 0003 left requeue out of v1, and said it waits for the worker daemon. The
reason it gave is still true: when the client dies, nobody is left to read the
job's output, so replaying it would encode frames for no one.

A different failure remains and it needs no dead client. The client is alive,
the worker's link or daemon fails before ffmpeg runs, and the client returns a
transport error. Jellyfin then sees ffmpeg fail and retries the request itself,
possibly on `localhost`, where VAAPI does not exist. That is rffmpeg defect 2
at a smaller scale: one transient failure reaches the viewer.

The hard part is telling "the worker never ran it" from "it ran and then
broke". A retry is only safe in the first case, because the second one has
already written bytes to the output path and a second attempt would mix two
encodes in one directory.

## Decision

The agent's `ready` frame draws that line. The daemon sends it after `spawn`
and before any output, so a failure to reach it means nothing ran and nothing
was written.

- `Error::NotStarted` means exactly that: the worker never started the job.
  The agent returns it when the connection, the hello or the wait for `ready`
  fails.
- `main.rs` retries such a job once, on another host, with a fresh job id. It
  marks the failed host down first, so placement cannot pick it again. The
  retry costs no extra wait beyond the normal queue wait.
- A job that fails after `ready` keeps today's behaviour: one transport error,
  one `HostDown` event, no retry.
- If no host is left for the retry, the client reports the original transport
  error. It does not fall back to local, because a worker did answer and
  hiding that would put the transcode on the wrong CPU.
- SSH keeps reporting exit 255 as a plain transport error. It cannot say
  whether the remote command started, and a guessed retry could duplicate
  output.

## Consequences

- A lost handshake on one worker costs one retry instead of a failed stream.
- Cost: one extra queue wait on the retry, and one journal row per attempt (the
  new job id keeps the counters and `rrffmpeg-stats` honest). A retry on a slow
  link doubles the delay before the first frame.
- The evidence is only as good as the `ready` frame. A daemon that sends
  `ready` and then fails to spawn would be retried after having spawned
  nothing, which is harmless, or after having written something, which would
  be a bug in the daemon.
- SSH hosts get no retry. They keep Jellyfin's own retry, and the local
  fallback that comes with it.

## Alternatives rejected

- Retry every transport error. A half-written HLS directory would receive a
  second encode. Jellyfin tolerates that as badly as we would.
- Retry SSH exit 255 by checking the pid file after the fact. The pid file
  answers "is it running now", not "did it run and write".
- Keep the orphan alive and let a later client adopt it. No client wants
  another client's output, and the frames would be stale. ADR 0003 already
  covers why the daemon kills the orphan instead.
