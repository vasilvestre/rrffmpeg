# 0007: Per-job pid file for liveness

Status: Accepted. For hosts with `agent_addr`, ADR 0009 answers `job_alive`
from the daemon's registry instead of the pid file.
Date: 2026-11-17

## Context

The janitor must decide whether a job recorded as active still runs on the
worker. The dispatcher holds the local ssh child process, not the remote
ffmpeg. A `SIGKILL`ed client leaves both the ssh client and the remote ffmpeg
orphaned, and no local handle remains after a restart.

Matching a process by name is unreliable: the worker runs several ffmpeg
processes, and two transcodes can share the same argv shape.

## Decision

Before `exec`, the remote command writes the pid it is about to become:

```sh
mkdir -p /tmp/rrffmpeg && echo $$ > /tmp/rrffmpeg/<job id>.pid && exec 'ffmpeg' ...
```

The janitor checks it with `kill -0 "$(cat <pid file>)"`. A missing pid file
makes `cat` fail, the substitution empty, and `kill -0 ""` fail, so a lost job
reads as dead by default. Job ids are unique across restarts
(`nanoseconds-pid`), so a stale file cannot be mistaken for a live job.

## Consequences

- Liveness needs no daemon, no socket, and no extra state on the worker beyond
  a small file per job in tmpfs.
- A pid file that outlives its process reads as dead only if the pid is gone.
  If the OS reuses that pid for an unrelated process within one 10 second
  cycle, the job looks alive and is reaped one cycle later. The window is
  small and the cost is a delayed reap, not a wrong dispatch.
- The file is never removed. Removing it needs a second SSH round trip, which
  costs more than a tmpfs inode. Files accumulate until the worker reboots.
  `shortcut: pid files accumulate in pid_dir until the box reboots, prune them
  when that becomes a problem.`
- The pid directory is `/tmp/rrffmpeg` by default and is configurable on the
  `Ssh` value, not in the TOML yet.

## Alternatives rejected

- `pgrep -f` on the command line. It matches the wrong process when two jobs
  share a shape, and it matches the ssh wrapper.
- A heartbeat in the journal written by the worker. It needs the agent
  transport, which does not exist in v1.
- Killing the ssh child on drop and trusting it to kill the remote ffmpeg. A
  dropped TCP connection does not reliably kill a remote process.
