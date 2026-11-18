# Architecture decision records

Each record states one decision that shapes the code, why it was taken, and
what it costs. The format is Michael Nygard's: context, decision, consequences.

Write a new record when a choice is hard to reverse, when it contradicts an
obvious alternative, or when someone would otherwise re-litigate it. Supersede
a record instead of editing its decision: change the status to `Superseded by
ADR-NNNN` and add the new record.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-stand-in-for-ffmpeg.md) | Stand in for `ffmpeg` and `ffprobe`, pass arguments unchanged | Accepted |
| [0002](0002-journal-off-the-critical-path.md) | Append-only journal and a background flusher, not a synchronous DB | Accepted |
| [0003](0003-in-process-janitor.md) | Repair in process, not with a cron watchdog | Accepted |
| [0004](0004-prefix-path-mapping.md) | Longest-prefix path mapping in both directions | Accepted |
| [0005](0005-ssh-transport.md) | SSH is the v1 transport, behind a trait | Accepted |
| [0006](0006-placement-caps-and-wait.md) | Per-host concurrency plus capability match, wait then local | Superseded by [0008](0008-cross-process-slots.md) |
| [0007](0007-pid-file-liveness.md) | Per-job pid file for liveness | Accepted |
| [0008](0008-cross-process-slots.md) | Cross-process slot files and an expiring down verdict | Accepted |
| [0009](0009-agent-transport.md) | An agent daemon as a second transport | Accepted, transport superseded by [0010](0010-agent-tls.md) |
| [0010](0010-agent-tls.md) | TLS with one pinned certificate, one connection per direction | Accepted, amended by [0018](0018-one-thread-relay-and-lazy-stdin.md) |
| [0011](0011-journal-timings.md) | The journal carries its own timings, read back by `rrffmpeg-stats` | Accepted |
| [0012](0012-shared-queue-with-client-weight.md) | A shared ticket queue orders starts, with a weight per client | Accepted, amended by [0017](0017-queue-decides-at-the-door.md) |
| [0013](0013-retry-a-job-that-never-started.md) | Retry a job that never started on another host | Accepted |
| [0014](0014-ssh-connection-persistence.md) | Keep the SSH connection open between jobs | Accepted |
| [0015](0015-replay-waits-for-the-janitor.md) | Replay the journal only when the janitor reads it | Accepted |
| [0016](0016-verifiable-install-and-update.md) | A version output, a tag check, and one install script | Accepted |
| [0017](0017-queue-decides-at-the-door.md) | The queue decides at the door, and passes free slots in a batch | Accepted |
| [0018](0018-one-thread-relay-and-lazy-stdin.md) | One daemon thread relays a job, and the stdin connection opens on use | Accepted |
| [0019](0019-refuse-the-local-fallback-for-ffmpeg.md) | Refuse the local fallback for a transcode, keep it for a probe | Accepted |
| [0020](0020-ssh-arguments-in-config.md) | SSH arguments in the config, not in a wrapper script | Accepted |
| [0021](0021-per-host-environment.md) | Per-host environment, and agent protocol 2 | Accepted |
| [0022](0022-progress-sample-in-the-journal.md) | The journal records fps, speed, content and copy-or-encode per stream | Accepted |
| [0023](0023-the-sample-is-a-peak-not-a-tail.md) | The recorded fps and speed are the highest running average, not the last line | Accepted |
| [0024](0024-declared-capabilities.md) | Capabilities are declared per host, with one token per real driver limit | Accepted |
| [0025](0025-sequence-numbers-are-reserved-per-process.md) | Sequence numbers are reserved per process, under a lock at open | Accepted |
| [0026](0026-a-block-is-exclusive-and-replay-never-moves-the-writer.md) | A journal block is exclusive, and replay never moves the writer | Accepted |

All records dated 2026-11-17.
