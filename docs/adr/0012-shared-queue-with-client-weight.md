# 0012: A shared queue decides who starts next

Status: Accepted, amended by ADR-0017
Date: 2026-11-17

## Context

Two things were missing from placement, and both come from the same real
situation: several Jellyfin clients can reach one worker that has the GPU.

1. Order inside one client. A `ffprobe` runs in tens of milliseconds and a
   transcode for minutes. When both wait for the last slot, the probe should
   go first, because the player is blocked on it.
2. Share between clients. A client that drives two players and a client that
   drives one share a worker with `concurrency = 2`, and nothing said which of
   them should wait. rffmpeg has no notion of it either. The seedbox world
   solved the same question for disks: BFQ weights on `blkio.bfq.weight`, 1 to
   1000, work-conserving, and the weight only matters while two tasks compete.
   A dedicated box at 700 and an idle one at 1 still get 1 every time it is
   alone.

The existing code had priority as a poll rate: a probe polls at 20 ms and a
transcode backs off to 400 ms. That is a hint, not an order. It breaks exactly
when it matters, since a slow poller loses even when its job is more urgent,
and it gives no way to express a weight: the range that polling can cover is
20:1, and the useful weight range is 1000:1.

## Decision

A queue of ticket files under `state_dir/coord/<host>/queue/`, next to the
slot files, with no daemon.

- A job that fails its first slot attempt publishes one ticket per eligible
  host: `coord/<host>/queue/<nanos>-<pid>.want`, holding
  `class weight seq client`. It refreshes the ticket's mtime on every poll,
  including while every eligible host is full, and removes the ticket when it
  exits, by any path, because the file is owned by a guard. Refreshing only
  against a host with a free slot drops the ticket as dead while a long job
  holds that slot: a transcode holds a slot far longer than 3 s.
- Before taking a free slot, a waiter lists the directory and takes the slot
  only when no other live ticket ranks before its own. Nothing enforces the
  order: each waiter declines on its own. A killed process needs no repair,
  because a ticket nobody refreshed for 3 s is ignored and deleted.
- Rank is class first, then the client's share of recent starts,
  `starts / client_weight`, compared by cross-multiplication so a wide weight
  range needs no floating point. Arrival time breaks ties, then the file name.
- Starts are counted in `coord/<host>/queue/served`, incremented under an
  flock on `served.lock` and rewritten by rename, so a reader never sees half
  a file.
- Only a job that had to wait is charged. A client that finds a free slot at
  once never touches the queue directory and never builds a lead over a client
  that shows up later. Counts older than 60 s are ignored, which is what keeps
  a client that ran alone for a month from starving the next one.
- Config: `client_name` (default: hostname) and `client_weight` (default 100,
  clamped to 1..=1000, the same scale as the kernel's IO weight).

## Consequences

- The weight only works if the clients share `state_dir`. Run two clients with
  private state dirs and each sees a worker that is always free, so the weight
  means nothing. This is not a new requirement, the slot cap already had it,
  but it is now visible in a second place.
- Every waiting job lists a directory, reads each ticket and reads the start
  counts, once per poll, up to 50 times a second for a probe. With two or three
  waiters on tmpfs that is microseconds, but it is no longer only `flock`.
- A job that waits is charged whether it eventually wins on this host or not.
  The count is per host, so a job that moves to another host leaves its old
  host's books slightly wrong for up to 60 s.
- The queue measures classes by `Job::priority`, a single integer, so a third
  class means inventing a number in `main.rs`. There is no per-client class.
- Fairness is best effort across a shared mount. Client clocks decide the
  arrival order, and a skew of more than a few seconds shows up as one client
  winning ties it should not.
- A worker serving jobs from clients that do not share the state dir has no
  order at all, but keeps working: every waiter sees an empty queue and takes
  the first free slot, as before.
- The old poll-rate difference stays. It is now what keeps a waiter honest
  after it becomes first in line, not what decides the order.

## Alternatives rejected

- A coordinator daemon. It would centralize the decision but brings back the
  thing this project exists to avoid: a process that must be up, supervised,
  and restarted, on the client that has no GPU. It also doubles the number of
  processes per job.
- Strict priority over a single global weight (what rffmpeg's `priority` flag
  does in the other direction). One number cannot express both "a probe before
  a transcode" and "this client gets four times the compute of that one".
- cgroup `blkio` or `cpu.weight` on the worker, the pulsedmedia approach. That
  shapes a running job, not the order of starts, and it needs root on the
  worker, a cgroup tree per client, and the worker to know which client a job
  belongs to.
- Kernel-level `flock` ordering, taking the slots in a fixed order and letting
  the kernel wake waiters in FIFO. `flock` has no fair queue across processes
  and no way to pass a class or a weight.
