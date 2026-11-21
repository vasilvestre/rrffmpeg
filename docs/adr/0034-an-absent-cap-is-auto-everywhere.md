# 0034 An absent cap is `"auto"` on every host

Status: Accepted
Date: 2026-11-21

## Context

ADR 0033 made an absent `concurrency` mean `"auto"` on a `local = true` host,
and 1 on every other host. The reason it gave was that sizing a remote host
from the client's journal "would export a guess across a network boundary".

That reason does not hold. `sizing::samples` keys every finished transcode by
the `host` that ran it, and reads the speed from `stats.speed`, the figure
ffmpeg printed on the machine that encoded. A worker's samples therefore
describe the worker, not the client. An explicit `concurrency = "auto"` on a
remote host already sizes it from its own history, and nothing rejected it.

So the difference between `"auto"` and an absent key was whether the operator
typed the word. Meanwhile the docs and the example config shipped
`concurrency = 2`, the guess ADR 0033 measured as wrong on the reference box: a
UHD 630 encodes one 1080p HEVC job at 1.43x and two at about 0.7x each.

## Decision

- An absent `concurrency` is `"auto"` on every host, remote workers included.
  The parsed struct still starts at 1 before `Config::sized` runs, so a config
  built in code and never sized keeps one slot.
- The startup line says `concurrency left out, so it is sized from that host's
  journal` for every host. `light_concurrency` keeps its default of 2 on a
  remote host and still follows the core count on a local one.
- The docs, the example config and the public page show `concurrency = "auto"`
  and say the cap starts at 1.

## Consequences

- A fresh install runs one transcode at a time until ten finishes of the class,
  each longer than 60 s, clear 1.3x. On a home server that is days, not hours.
  The alternative, a hand-set 2, oversubscribes the reference GPU from the first
  job.
- Existing configs that omit the key change behaviour on upgrade: 1 becomes
  measured. The cap can only reach what that host's own tail supports, and a
  class that has no sample still pins the host at 1. The release notes for the
  next version must say so.
- The record format, the slot files, the queue and the placement order are
  untouched. This changes a default, not a mechanism.
- `light_concurrency` keeps 2 on a worker: no measurement backs a light job's
  cost, and a light job lasts seconds.

## Alternatives rejected

- Keeping 1 on a remote host until the operator writes `"auto"`: the example
  config then teaches the guess ADR 0033 exists to remove.
- Refusing an absent key: it breaks every config written before this change,
  for a value the code can compute.
- Sizing a worker from the client's own speeds: that is the guess ADR 0033
  rejected, and it is not what the journal holds.
