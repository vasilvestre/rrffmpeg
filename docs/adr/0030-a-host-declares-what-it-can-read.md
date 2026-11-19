# 0030: A host declares the roots it can read

Status: Accepted

Date: 2026-11-19

## Context

Placement chose a host from three facts only: the token list, the down flag
and a free slot. Whether the host could open the input file was not one of
them. A path map rewrote the arguments after the lease was taken
(`src/main.rs`), so a map never influenced the choice.

The observed install declares two hosts: an archive box that holds the media,
then a worker with the GPU. A title only the archive box can read is placed on
the worker, because the worker has a free slot and the archive box does not win
the ratio. ffmpeg on the worker cannot open the input, exits 254, and the
dispatcher reports that code. ADR 0013 forbids a retry, correctly: the job
produced output before it failed, and a second attempt could double-write the
same segments. The operator's workaround was to reorder the `[[hosts]]`
blocks, which is the bug this record removes.

## Decision

A host states the roots it can open:

```toml
[[hosts]]
name = "archive"
reads = ["/mnt/archive"]
```

- Entries are absolute paths **as the host sees them**, after its own path
  maps. A host that maps `/home/alice` to `/home/bob/media` and lists
  `/home/bob/media` covers every input the client sends from `/home/alice`.
- Placement maps each input argument for the candidate host with the same
  `paths::to_worker` a job gets, then asks whether the result starts with one
  of the roots. Mapping first is the whole point: a root written in client
  terms would cover nothing.
- The inputs it reads are the value after `-i`, plus the attached form
  (`-i/home/alice/film.mkv`). Several inputs must all be covered; one outside
  the roots skips the host.
- A token that is not an absolute path (`http://`, `pipe:0`, a relative name)
  is not a root this record judges, so it passes. A remote stream would
  otherwise exclude every host that declares `reads`.
- The check runs in `Scheduler::eligible`, which is the filter for `pick_host`
  and for the queue lanes. A host that cannot read the job earns no ticket,
  so it also stops blocking the waiters behind it.
- An absent or empty `reads` accepts any input, which is what every config
  written before this key means. A config that declares no `reads` routes
  exactly as it did before the upgrade.
- The config refuses a relative entry. It would match no absolute input, and
  the host would silently take no job at all.
- A `local = true` host rewrites no path, so its entries are the client's own
  paths; leaving `reads` out is the same thing, since the client can open what
  it can open.

## Consequences

- The archive-only title lands on the archive box, and the GPU worker keeps
  taking what it can read. No reordering is needed, and the order stops
  mattering (ADR 0031).
- A wrong `reads` entry is now a routing decision instead of a runtime error:
  a host whose roots miss the media takes no job, and the other host carries
  the load. The janitor does not notice, so a typo costs throughput, not
  correctness.
- The declaration is the operator's claim, not a proof. Two hosts can both
  cover the same client prefix, one natively and one through a copy, and only
  one of them holds a given file. The rule cannot tell them apart: both map
  the input under a root they list, so both stay eligible. When the client's
  namespace does not say which box holds a title, split the prefix per library
  on the client, or keep a single host for that prefix. `prefer` does not help
  here, because it never filters: the other host still takes the job when the
  first is full.
- A job whose input no host covers gets no lease, waits `queue_wait_secs`, then
  falls back to the local binary as before. It fails there for the same reason,
  but the log line now says no slot rather than one English ffmpeg error per
  attempt. Declaring the archive root on the archive box is what fixes it.
- The check costs one string compare per input per candidate host, on the
  dispatch path. Inputs are already parsed there, so there is no new process,
  no `stat`, and no probe. ADR 0006 rejected a per-job probe and this record
  keeps that rejection.
- The declared roots double as a health check. The janitor probe already stats
  every `paths` target within 5 s; it now stats every `reads` root too, once
  per host per cycle. A dropped FUSE mount therefore marks the host down
  instead of returning 254 to every job that lands there. That is the existing
  per-host probe, never a per-job one.
- Placement is now conservative: a host that could read the file through a
  mount it does not list, or through a symlink, is skipped.

## Alternatives rejected

- **A per-job `test -r` on the host.** ADR 0006 measured the cost of a per-job
  round trip and rejected it; a check that only answers for one path, after a
  slot is already taken, is worse than a declaration.
- **Deriving the roots from `paths`.** A map says where a file appears, not
  whether the mount is alive or complete. The two hosts in the failing install
  map the same client root to different places, and only one of them has the
  title.
- **Asking the worker's ffmpeg where it can read.** There is no such question,
  and the answer would be stale the moment a mount drops.
- **Failing the job and letting Jellyfin retry.** ADR 0013 forbids a retry of a
  job that produced output, and Jellyfin retries the whole transcode.
