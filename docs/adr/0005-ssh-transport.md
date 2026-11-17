# 0005: SSH is the v1 transport, behind a trait

Status: Accepted
Date: 2026-11-17

## Context

A worker daemon would remove the per-job SSH round trip and make requeue
natural, but it is a second binary to install, pair, and upgrade. SSH is
already there on every home lab box, and rffmpeg proves it is enough to move a
transcode.

Jellyfin writes `q`, `p`, `u`, and `c` to stdin, and some have no newline. It
parses ffmpeg's progress from stderr. Anything that merges the streams or
allocates a pty breaks playback.

The arguments come from Jellyfin and contain spaces, quotes, `$`, backticks,
semicolons, and filenames with newlines. A dispatcher that builds a local shell
command corrupts them.

## Decision

Define a `Transport` trait with `run`, `probe`, and `job_alive`. Ship one
implementation, `Ssh`, and keep the trait so an agent can be added later
without touching the scheduler.

`run` builds one remote shell command string and passes it to ssh as a single
argument:

```text
ssh -T -o ServerAliveInterval=15 -- worker '<remote command>'
```

- `-T` disables pty allocation, so Jellyfin's raw stdin bytes pass through.
- `stdin`, `stdout`, and `stderr` are inherited. Nothing is read, buffered, or
  merged, except `2>&1` when `stderr_as_stdout` is set.
- Each argument is single-quoted for a POSIX shell. The only escape is for a
  single quote itself.
- The remote command is `mkdir -p <pid dir> && echo $$ > <pid file> && exec
  <program> <args>`. `$$` is expanded by the remote shell before `exec`
  replaces it, so the pid file holds the ffmpeg pid.
- A host may override `ffmpeg_bin` or `ffprobe_bin` to point at a wrapper that
  sets the OpenCL environment.
- Exit code 255, or a signal with no code, is an ssh failure, not an ffmpeg
  result. It returns a transport error, the host is marked down, and no
  `Finished` event is recorded.

## Consequences

- Zero install on the worker: an SSH account and ffmpeg are enough.
- One ssh process per job, plus one full SSH connection. `ServerAliveInterval`
  keeps a long transcode from being dropped by an idle NAT.
- The remote side must run a POSIX shell. Windows workers are out of scope.
- A non-UTF-8 argument is refused with an error, because the remote command is
  a UTF-8 string. Local mode still handles such names.
- `probe` and `job_alive` use `BatchMode=yes` and a connect timeout, so a
  stalled host answers `false` quickly instead of hanging the janitor.
- An agent transport later replaces the three methods, not the call sites.

## Alternatives rejected

- A pty. It echoes input, translates line endings, and loses raw bytes.
- Building a local argv for ssh. ssh joins its arguments into one command
  string anyway, so quoting must be explicit; hiding it invites quoting bugs.
- The worker daemon in v1. It is more work than the dispatcher itself, and
  SSH already ships everywhere.
