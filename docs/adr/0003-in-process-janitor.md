# 0003: Repair in process, not with a cron watchdog

Status: Accepted
Date: 2026-11-17

## Context

rffmpeg trusts the newest state row per host and never re-tests it. One
transient SSH timeout marked the only GPU host `bad` for the life of the
process and caused a full outage. A separate cron job had to clear the row.

The same model leaks orphan rows: state is removed from a signal handler, and
`SIGKILL` skips it. A dispatched ffmpeg can also outlive its client.

The repair work is small and periodic. It does not need a second program,
a lock file, and an install step on every client.

## Decision

Spawn one janitor thread with the dispatcher. It runs one cycle every 10
seconds and does two things, in this order:

1. **Reap.** For each active job, ask the worker whether its process still
   runs. A job is judged only after a previous cycle saw it, so a job that
   started between its `Started` event and its remote pid file cannot be
   reaped by mistake. A job reported dead gets a `Reaped` event.
2. **Reconcile.** Probe every configured host once per cycle, then flip its
   flag and record the transition (`HostUp` or `HostDown`).

Reaping runs first because a stale active row is what silently undercounts
slots.

The thread never exits on error and never panics. `stop` wakes the sleep with a
condvar and joins, so a one hour interval does not delay shutdown.

## Consequences

- A dead host comes back without an operator: the next successful probe marks
  it up and wakes waiting jobs.
- A `SIGKILL`ed job leaves an active row for at most two cycles.
- Cost: one SSH probe per host per cycle, plus one liveness check per active
  job per cycle. The per-job check is what keeps the orphan repair honest.
  The probe is per host, never per job, which is the rffmpeg defect this
  avoids.
- Requeue is not implemented in v1. A client that dies takes its job with it,
  and no agent is left to retry it. That waits for the worker daemon.
- The janitor shares the process with the dispatcher, so a hung janitor cannot
  be repaired by the dispatcher. Probes and liveness checks carry their own
  timeouts for that reason.

## Alternatives rejected

- Newest-row-wins with no retest. That is the outage.
- An external cron watchdog. It repairs what the program refuses to repair,
  and every client needs it installed.
- A separate supervisor process. Adds a process model, a socket, and a
  deployment step for two small loops.
