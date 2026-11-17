# 0009: An agent transport, one TCP connection per job

Status: Accepted. The transport itself is superseded by
[0010](0010-agent-tls.md): TLS, and one connection per direction. The frames,
the registry and the exec rules below still stand.
Date: 2026-11-17

## Context

ADR 0005 shipped SSH and kept a `Transport` trait for a later agent. The
measured cost of SSH in production is small but not zero: one process, one
connection, and a key exchange per job, plus a per-job health probe. A dropped
ssh link also makes the job unknowable: the client cannot ask the worker
anything once its connection is gone, and `job_alive` needs a second ssh.

A daemon on the worker removes all three: no per-job handshake, liveness held
in one place, and a probe that is one small request instead of a shell.

The hard part is not the control channel, it is the data channel. Jellyfin
writes raw bytes to stdin, some of them without a newline, and reads ffmpeg's
progress from stderr. Both streams must arrive byte-exact and in order, and the
exit code must not overtake the last line of stderr.

## Decision

Add `src/agent.rs`: a frame protocol and a daemon, `rrffmpeg-agent`.

Frames are 5 bytes of header and a body: one kind byte, then a big-endian
`u32` length. The kinds are `hello`, `stdin`, `stdout`, `stderr`, `exit`,
`stdin_eof`, and `status`. A body over 1 MiB is a protocol error.

One connection per job:

1. The client sends `hello`: a token, the op, the job id, the program path and
   the arguments as a JSON array.
2. The daemon checks the token in constant time, then spawns the program with
   piped stdio.
3. Two daemon threads copy stdout and stderr into frames. The client's stdin
   pump sends `stdin` frames and one `stdin_eof`.
4. When the child exits, the daemon waits for both pipes to reach EOF, then
   sends `exit`. Sending the code earlier loses the last stderr line: that bug
   is what the first version of the test caught.
5. The client writes stdout frames to stdout and stderr frames to stderr, or
   to stdout when `stderr_as_stdout` is set. Merging happens on the client, so
   the daemon has no special case.

A `status` connection carries the mapped roots to stat and an optional job id.
It answers `{ "ok": bool, "alive": bool }`. `probe` and `job_alive` both use
it, so the same mount check as SSH applies.

A job is registered in a daemon-wide map from job id to pid, and removed after
`wait`. If the client connection closes while the child lives, the daemon sends
`SIGTERM`, waits two seconds, then `SIGKILL`, and the waiter thread reaps. This
is stricter than SSH: a killed client does not leave a worker transcode behind
for the janitor to find.

Routing is per host. `Router` in `transport.rs` sends a host with `agent_addr`
to the agent and everything else to SSH, so a config can mix both. The token
comes from `agent_token` or `RRFFMPEG_AGENT_TOKEN`; the environment wins. A
host with `agent_addr` and no token is a startup error, not a silent fallback
to local.

## Consequences

- No TLS. The token crosses the network in clear text and the daemon executes
  whatever argv the token holder sends. Bind the listener to a private
  interface, WireGuard or Tailscale, and keep it off the LAN. Upgrading to
  rustls is a contained change in this one module.
- The daemon is a second binary to install and keep running, with a systemd
  unit. `--token` is visible in `ps`; prefer the environment.
- The job registry lives in the daemon's memory. A restarted daemon reports
  every job dead, and the janitor reaps rows the worker really finished. The
  journal and the client's pid logic still agree after that.
- The daemon does not cap concurrency. The client's slot files do, and a rogue
  client can exceed them. Add a per-host cap in the daemon if that matters.
- Arguments are carried as JSON, so a non-UTF-8 argument is refused. SSH has
  the same limit for a different reason.
- A pid race remains on kill: the pid is checked against the registry before
  signalling, and a pid reused inside that microsecond window would be killed.
  The window is far smaller than the pid-file reap it replaces.
- One frame pump per stream per job, so a job costs three threads on the
  daemon instead of one process.

## Alternatives rejected

- MQTT or RabbitMQ. They carry the control plane, not the media stream. A
  broker cannot move a transcode's stdout, and copying dffmpeg's transport list
  would ship a feature that does not work.
- HTTP with chunked bodies per job. It needs a framing layer on top anyway,
  and then the request/response shape fights the bidirectional stream.
- A Unix socket. It works only when the worker shares the client's filesystem,
  which is the case the dispatcher exists to avoid.
- Keeping `job_alive` on pid files over SSH. That is the cost the agent
  removes, and it is wrong the moment the client dies.
- mTLS in v1. Correct, but rustls, certificates and rotation triple the work
  for a home lab link that already runs inside a VPN. Recorded as the upgrade
  path, not as the v1.
