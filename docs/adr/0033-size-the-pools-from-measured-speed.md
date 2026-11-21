# 0033 Size the pools from measured speed

Status: Accepted, amended by ADR-0034

## Context

The pool caps are numbers an operator guesses. On homebase the guess is better
than most: `concurrency = 2` matches a UHD 630 that encodes one 1080p HEVC
stream at 1.43x with headroom and two at about 0.7x each. A different library
would need a different number, and nothing tells the operator when the guess
went wrong.

The journal already holds what the guess is trying to estimate. Every finished
job can carry the speed ffmpeg reported, the width and height of the source, the
output codec, and `in_flight` at the moment it started. Eight jobs in the
production journal show the failure directly: 2160p HEVC to `wrapped_avframe` at
0.176x while IntroSkipper enumerated the library. That is a full decode thrown
away, and it ran beside playback.

Throttling is the constraint that makes naive sizing dangerous. Jellyfin pauses
a stream by writing to the ffmpeg child's stdin. The process stays alive, keeps
its slot file, and uses no CPU, so a cap that lowers itself whenever throughput
drops would sink to 1 while three viewers are watching and never climb back.

## Decision

- A host declares `concurrency` as an integer or as `"auto"`. An absent key
  means `"auto"` on a host with `local = true`, and 1 on every other host, with
  one warning line at startup. Sizing a remote host from the client's own speed
  history would export a guess across a network boundary, which is worse than 1.
  `light_concurrency` takes the same syntax: `"auto"` gives the light cap, and
  an absent key keeps its default of 2 on a remote host and gives the light cap
  on a local one. `0` and `"unlimited"` are a config error: on this box
  "unlimited" means admitting jobs the GPU cannot serve, and the bill lands on a
  live viewer.
- The caps are resolved once, when the process starts, before anything reads
  them: a cap that moved under a running job would be a lie. One process serves
  one job, so that start is the admission the cap describes.
- The heavy cap is a pure function of journal samples for that host. One host
  has one `concurrency`, so the cap it keeps is the smallest over its classes:
  it has to hold whatever Jellyfin sends next, and the class that set it is
  what the `Sized` line names.
  1. Keep the last 200 finished jobs of the host that carry a speed, have the
     `transcode` kind, and ran longer than 60 s.
  2. Group them by `(src_codec, src_h, dst_codec, tonemap)`. Tonemap is read
     from `src_hdr`: an HDR source is the only one that needs it. The height is
     kept as reported, not rounded to hundreds: rounding 1080 gave "1000p" in
     the class name, and the class name is a label an operator reads.
  3. For the class being admitted, `solo_p05` is the 5th percentile of the
     speeds recorded at `in_flight = 1`. When every sample ran with a
     neighbour, scale each one: `speed * in_flight`.
  4. Predict `solo_p05 / N` at N jobs, because a GPU-bound box has roughly
     constant total throughput.
  5. `cap = clamp(floor(solo_p05 / 1.3), 1, ceiling)`, with
     `ceiling = min(4, cpus / 2)` and never below 1. The 1.3 keeps one job of
     headroom for whatever else shares the box.
- Hysteresis is derived from the same journal tail, not stored: no file under
  `coord/`, no lock, no counter. The base cap comes from the percentile, then
  the last 10 finishes of the class raise it by 1 when all reached 1.3x or
  better, and the last 3 lower it by 1 when all came in under 1.0x. Because the
  base is recomputed from the tail on every run, two processes that admit jobs a
  second apart compute the same answer, so the rule needs no rate limit and
  cannot ratchet. A running job is never touched.
- No sample for the class means cap 1, and one log line naming the class.
- The light cap is `min(4, max(2, cpus / 2))` under `"auto"`, fixed, no
  hysteresis: a light job lasts seconds and is cheaper than the queue it would
  otherwise wait in.
- Slot files are taken up to the resolved cap, so a cap that moves up and down
  keeps the same file names. A parked job holds a spare file beyond the cap,
  one per parked job, bounded by `ceiling`.
- Parked jobs: the local transport samples `/proc/<pid>/stat` every 30 s for the
  child it spawned. A job that used under 1% of one CPU for 60 s while its
  output stopped growing is parked. It keeps its slot file and stops counting
  toward the heavy cap, up to `ceiling` extra admissions. The journal gets one
  `Parked { job }` on the way in and one `Resumed { job }` on the way out.
- Every cap change writes `Sized { host, class, cap, samples, p05, previous }`,
  one line, with the reason in the same record.

## Consequences

The cap follows the library instead of a guess, and the journal says why it
moved. The costs:

- Sizing needs history. A fresh state dir, a new codec or a new class starts at
  1 and climbs one job per ten good finishes.
- The unit of measurement is the class, so a 2160p HEVC job and a 1080p H.264
  job size separately. That is intended, and it means a rare class stays at 1
  longer. It also means one slow class pins the whole host: a library that
  holds 4K HDR titles keeps `concurrency` at the 4K cap even when the current
  job is 1080p. The alternative, a cap per class at admission, needs placement
  to size on every job, which is a change to the hot path; the safe bound wins
  for now.
- The parked sampler costs one thread per local job and one `/proc` read every
  30 s. A remote or agent job is never sampled, so a throttled job there still
  counts, which is the old behaviour.
- A cap of 1 that the operator wanted higher is now a diagnostic: the `Sized`
  line names the p05 that produced it, and `rrffmpeg-stats` prints one line per
  host and class with the cap, the sample count and the p05 behind it.
- Config semantics change for an absent key on a local host. The release notes
  and `config.example.toml` must say so.

## Alternatives rejected

- An `unlimited` mode: it accepts work the GPU cannot serve, and the failure is
  a viewer's stream.
- Sizing from the client's `nproc` alone, without speed: cores say nothing
  about a fixed-function encoder.
- Lowering the cap on any slow finish: a throttled stream is slow and alive,
  and lowering for it converges to 1. That is why parked jobs are detected
  before the speed is read.
- Killing or migrating a running job to enforce a lower cap: a viewer loses the
  stream.
- Sizing a remote host from the local journal: the numbers describe another
  GPU.
