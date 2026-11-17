# 0014: Keep the SSH connection open between jobs

Status: Accepted
Date: 2026-11-17

## Context

ADR 0005 chose SSH as the first transport because a worker needs nothing
installed. That transport spawns one `ssh` per job, and each spawn pays the
whole handshake: TCP connect, version exchange, key exchange, authentication,
then the remote command.

The cost is measurable and large next to the budget. On one loopback box,
`ssh localhost true` takes 108 ms of wall clock, and a dispatch through the SSH
transport measured 154.6 ms p50 and 173.6 ms p99 against a local ffmpeg at
27.3 ms. rffmpeg, which sets `ControlMaster=auto` and `ControlPersist=300`,
measured 119.1 ms p50 on the same box: it opens a channel on an existing
connection instead of a new session.

The agent transport avoids this entirely, at 31.0 ms p50, but it needs a daemon
and a listening port on the worker. A worker that cannot run one is stuck with
SSH and paid the handshake on every job, including every Jellyfin startup
probe.

## Decision

The SSH transport passes three options to `ssh` when persistence is on:

```text
-o ControlMaster=auto
-o ControlPath=<state_dir>/ssh/%C
-o ControlPersist=<ssh_persist_secs>
```

- `ssh_persist_secs` is a top-level config key, default 300, matching rffmpeg.
  Zero removes the three options and restores one process per job.
- The socket directory is on tmpfs, mode 0700: `$XDG_RUNTIME_DIR/rffmpeg` when
  the session provides one, else `/dev/shm/rrffmpeg-<uid>`, else
  `state_dir/ssh`. `ssh_socket_dir` overrides the choice. A control socket is
  tied to one boot and one login, so a file that survives a reboot is a lie,
  and ssh only trusts the local user anyway.
- The directory must be owned by the running user and must not be a symlink.
  Refusing anything else costs one `stat` and closes the obvious local attack:
  plant a path, then own every connection that multiplexes through it.
- `ControlMaster=auto` makes a dead or stale socket a fallback, not a failure:
  ssh opens a new connection when the master is gone.
- The options apply to the three SSH call sites: the job, the probe and the
  liveness check. All three were separate handshakes before.

## Consequences

Measured on the same loopback box, real ffmpeg 8.0.1, N=30 before and N=60
after:

```text
SSH, persistence off   p50 155.60 ms  p99 173.55 ms
SSH, persistence 300   p50  37.94 ms  p99  46.94 ms
agent                  p50  30.95 ms  p99  33.92 ms
```

A dispatch over SSH costs about 4 times less, and lands 7 ms from the agent on
a LAN. The 50 ms budget now holds on SSH.

The cost is a long-lived ssh process and an sshd session per host on the
worker. They expire after `ssh_persist_secs` without a job. A worker that
reboots while the socket exists is handled by `ControlMaster=auto`: the next
job pays a handshake, then multiplexes again. A socket file left in a read-only
or full `state_dir` degrades to one process per job, with a warning on stderr.

ssh derives each socket name from a hash of user, host and port (`%C`), which
stays under the socket path limit.

The socket location does not change the latency. Same box, same config, only
`ssh_socket_dir` differs:

```text
socket on tmpfs (/run/user/1000)   p50 37.96 ms  p99 42.43 ms   N=60
socket on ext4 (~/.cache)          p50 38.00 ms  p99 39.78 ms   N=60
```

That is inside the noise: ssh opens a unix socket, and a socket inode costs
the same on tmpfs and on a cached disk path. tmpfs wins on hygiene, not speed:
`/dev/shm` and the runtime directory are emptied on reboot or logout, so no
stale socket outlives the sshd session it points at.

Two clients of the same user and runtime directory share one master per host.
That is safe and saves a second handshake. A different user gets a different
directory, and therefore its own connections.

## Alternatives rejected

- **Agent only.** Requires a daemon and an inbound port. ADR 0005 keeps SSH for
  workers where that is impossible, so the SSH path still needs to be fast.
- **A wrapper `ssh` script per host.** Works, but it hides the transport
  settings outside the config file and outside the tests. The options belong to
  the transport the code owns.
- **`~/.ssh/config`.** The dispatcher would inherit whatever the user set, with
  no default and no way to see it from `rrffmpeg`. A config key is testable.
