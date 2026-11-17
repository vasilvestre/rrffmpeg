# 0008: Cross-process slots, and a down verdict that expires

Status: Accepted
Date: 2026-11-17

## Context

Jellyfin forks one rrffmpeg process per job. ADR 0006 kept the per-host slot
count in process memory and the host-down flag in the same scheduler. Both are
per process. Production therefore had no concurrency cap, and a host marked
down by one job was still up for the next job's process. The design text said
"cap", the code enforced nothing.

The wait also lived in memory: a `Condvar` queue inside one process. Jellyfin's
processes never meet in that queue, so the wait was invisible in production.

## Decision

Slots are files under `state_dir/coord/<key>/slot.<n>`, where `<key>` is the
FNV-1a 64 hash of the host name in 16 hex digits. The name is an ssh
destination and can contain `@` or `:`, so it never becomes a path component.

A slot is taken with an exclusive `std::fs::File::try_lock`. The `SlotGuard`
owns the `File`; `Drop` closes it and the kernel frees the lock. A panic, a
crash, or `SIGKILL` releases it the same way, so no janitor runs and no stale
lock exists. The file opens with `O_CLOEXEC`, so the ssh child cannot inherit
the descriptor and pin the slot after the client dies.

`occupancy` probes each slot by taking the lock and dropping it at once. An
unreadable slot counts as no load, so placement still admits the job.

`coord/<key>/down` holds an absolute expiry in milliseconds since the epoch.
`mark_down` writes a temp file, then renames it over `down`, so a reader never
parses half a number. `is_down` returns true only before the expiry, and
removes an expired or unparsable file.

`Scheduler::acquire` makes at most two passes over the host list, then returns
`None`, so a lost race cannot spin. `acquire_blocking` polls until a deadline:
20 ms for `priority < 0`, otherwise 40 ms doubling to 400 ms, with 0 to 10 ms
of jitter so peers do not retry in lockstep. The `Condvar` waiter queue is
deleted.

Every lock failure falls open. An unusable `state_dir` logs one line and the
job runs uncapped. A broken state dir must never stop a transcode.

## Consequences

- The cap now changes production behaviour. Jobs that used to start at once
  now wait. On a worker with `concurrency = 1`, the second job waits for the
  first to end.
- A job that finds no slot still waits `queue_wait_secs`, default 600, then
  logs one line and transcodes locally on a GPU-less client. That job usually
  fails. Set `queue_wait_secs` high enough, or Jellyfin's own queue will.
- When the client is killed, the kernel frees the slot but the ssh child and
  the remote ffmpeg can outlive it. The slot is then free while the GPU is
  still busy, so a new job can briefly oversubscribe the GPU. The janitor's
  pid-file reap narrows the window. It does not close it, because the client
  that would do the reaping is gone.
- `down_ttl_secs`, default 60, bounds how long a verdict is trusted. While a
  process is alive, its janitor retests each host once per cycle and clears the
  verdict. When the process that marked a host down has exited, expiry is the
  only repair left. This is the fix for the "a bad host stays bad forever"
  outage, and it needs no cron watchdog.
- `state_dir` must be on a local filesystem. On NFS `flock` can return
  `ENOLCK`, which falls open: the cap is silently lost, and the single log
  line is the only signal.
- Priority stays best-effort. It only sets the poll rate, so it does not order
  jobs across processes.
- The cap covers one `state_dir`. Two clients with separate state dirs do not
  see each other's slots.

## Alternatives rejected

- A running count in the journal. The journal is append-only and has no
  compare-and-swap, and a crash leaves a count that nothing corrects.
- A coordinator daemon handing out leases. That is dffmpeg's model and it needs
  the agent transport, which v1 did not have. The agent exists now (ADR 0009),
  but it is a data path, not a placement authority, so the slots stay in
  `state_dir`.
- Keeping the in-process `Condvar` queue. The waiters are in different
  processes and never meet.
- POSIX record locks (`fcntl`). They are owned by the process, not the
  descriptor, and any close of the file drops them. `flock` on a `File` is the
  behaviour wanted here.
- A janitor that deletes stale slot files. It cannot tell a stale file from one
  a live process holds, and guessing wrong kills a running transcode.
- A shared down counter instead of an expiry. Nothing decrements it after the
  processes that saw the failure have exited.
