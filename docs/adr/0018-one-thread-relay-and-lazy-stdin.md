# 0018: One daemon thread relays a job, and the stdin connection opens on use

Status: Accepted
Date: 2026-11-17

## Context

ADR 0010 made the agent speak TLS on one connection per direction: one for
`exec` and its output, one for `stdin`. The implementation paid for both on
every job, in two ways.

- The client opened the stdin connection right after `READY`, before knowing
  whether anything would ever be written. Almost nothing writes: a `ffprobe`
  never does, and a transcode writes at most a `q` when the user stops
  playback. So every job paid a second TCP connect and a second TLS handshake,
  on the client and on the daemon, plus a thread on each side.
- The daemon relayed output through three extra threads per job: a waiter that
  owned the child, and one pump per pipe, all feeding a bounded channel that
  the connection thread drained. A job therefore cost the daemon 4 threads, and
  every chunk was copied twice: once into a `Vec` for the channel, once into
  the frame.

Measured on 2026-11-17, one client, 400 runs, `ffmpeg_bin = /bin/true`, so the
whole cost is the dispatcher:

| | p50 | p95 | p99 | max |
|---|---|---|---|---|
| before | 1.70 ms | 1.82 ms | 2.01 ms | 3.06 ms |
| after | 1.59 ms | 1.69 ms | 1.81 ms | 1.98 ms |

Threads counted in `/proc/<agent>/task` one second into a 3 s job: 1 when idle,
4 with the old relay, 2 with the new one.

## Decision

- `stdin_thread` waits for the first byte from stdin and only then connects.
  A job that never writes stdin never opens the connection, and still gets both
  its ends: the thread drains the pipe to EOF and closes when the job ends.
- `exec_reply` owns the child itself: it spawns it, writes `READY`, then polls
  both pipes with `libc::poll` in the connection thread and writes each frame
  straight to the socket, with one reusable 32 KiB buffer. When both pipes
  report EOF it waits for the child, removes the slot, and sends `EXIT`. No
  channel, no pump, no waiter.
- The accepted socket gets `TCP_NODELAY`. `READY`, every output frame and
  `EXIT` are small writes, and without it Nagle holds each one until the
  previous is acknowledged, which a client on another machine answers after a
  delayed ACK, up to 40 ms. The client already set the option on its side.
- The child now belongs to the connection that started it, so a client that
  disappears kills its job (`abandon`): `SIGKILL` and reap before the slot is
  removed. Before this change the kill came from the stdin connection closing,
  which the lazy stdin connection no longer guarantees.

## Consequences

- Zero extra threads per job on the daemon for a job that writes no stdin.
- An abandoned job is killed at once instead of when a pipe eventually breaks.
- `SIGKILL` on abandon does not let ffmpeg finalize partial files. Nobody is
  left to read them, and Jellyfin discards the output of a dropped job.
- stdout and stderr now interleave in the order `poll` reports them, and both
  are written by one thread, so a frame cannot overtake another. The `EXIT`
  frame still comes last, after the pipes are empty.
- A job that keeps one pipe open after the other closes holds the connection
  until both report EOF, which was already true of the pump design.

## Alternatives rejected

- A thread pool in the daemon. A job holds its connection for its whole life,
  so a pool would starve as soon as the number of jobs exceeds it, and one
  thread per connection is what a Jellyfin worker needs anyway.
- Doing the TLS handshake in the accept thread before handing off. It removes
  the thread spawn from the handshake, but a client that connects and stalls
  would block every other accept.
- Keeping the eager stdin connection for symmetry. Symmetry cost a handshake
  and two threads per job for a channel that is silent in practice.
