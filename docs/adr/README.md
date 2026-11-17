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
| [0006](0006-placement-caps-and-wait.md) | Per-host concurrency plus capability match, wait then local | Accepted |
| [0007](0007-pid-file-liveness.md) | Per-job pid file for liveness | Accepted |

All records dated 2026-11-17.
