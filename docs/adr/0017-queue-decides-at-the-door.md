# 0017: The queue decides at the door, and passes free slots in a batch

Status: Accepted
Date: 2026-11-17

## Context

ADR 0012 gave waiting jobs a ticket queue. Measured on 2026-11-17 with 16
concurrent clients and `concurrency = 8` on one agent host, it left three holes
that only show up when a worker is oversubscribed, which is exactly the case it
was written for: 320 probes through the agent, 16 client processes in parallel,
same machine.

| Run | p50 | p95 | p99 | max |
|---|---|---|---|---|
| before | 2.60 ms | 3.82 ms | 42.90 ms | 43.77 ms |
| after | 4.21 ms | 4.67 ms | 6.45 ms | 12.66 ms |
| before | 2.50 ms | 3.78 ms | 292.67 ms | 314.66 ms |
| after | 4.30 ms | 4.66 ms | 6.33 ms | 12.53 ms |
| before | 2.50 ms | 3.68 ms | 130.78 ms | 303.95 ms |
| after | 4.28 ms | 6.38 ms | 12.40 ms | 31.36 ms |

Each pair is one run of each binary, back to back.

- p99 falls by 85 to 98 percent and the maximum by 90 to 96. p50 rises by
  1.8 ms, and that
  is the queue working: with twice as many clients as slots, half the jobs now
  wait their turn instead of jumping it.
- A dispatch with no waiter pays one `readdir` per eligible host, which fails
  with `ENOENT` while nobody queues. Measured on the same box, one client, 400
  runs: p50 1.70 ms before, 1.65 ms after, p99 1.86 ms before, 1.81 ms after.
- A waiter now competes with other waiters instead of with every newcomer, so a
  burst drains at about 2 ms per slot batch rather than per slot.
- Refreshing every 500 ms instead of every poll means a waiter is up to 500 ms
  away from being declared dead, still 6 times inside the 3 s rule.

## Alternatives rejected

- A signal instead of polling, such as inotify on the slot files. It would cut
  the 2 ms floor, but it adds a watcher per job, a fallback path for a watcher
  that fails, and a second way for a slot to be seen free. The order of
  magnitude does not justify it at these latencies.
- Letting the head of the queue only, but with the accept side of the slot cap
  widened for probes. That relaxes the concurrency cap, which is what the
  worker's GPU needs, to save a millisecond.
- Keeping the newcomer bypass and relying on a shorter poll. Measured: the
  waiter still loses to every newcomer, and the p99 tail is the number that
  proves it.
