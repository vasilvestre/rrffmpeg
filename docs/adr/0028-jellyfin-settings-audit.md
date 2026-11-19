# 0028: The Jellyfin audit reuses the placement rule

Status: Accepted
Date: 2026-11-19

## Context

Jellyfin states what it will encode in `encoding.xml`, and the worker proves
what it can encode in `capabilities`. Nothing compares the two. The failure is
silent in the wrong direction: Jellyfin reports a setting as enabled, the
dispatcher routes the job, and the job fails on the worker after taking a slot.
A setting that no host can serve looks exactly like a working one until a
viewer presses play.

The two files do not share a vocabulary. `encoding.xml` holds booleans and
enum values; the config holds tokens. A hand-written translation table would
be a second source of truth next to `capabilities::required` (ADR 0024, ADR
0027), and the two would drift the next time a token is added.

## Decision

`rrffmpeg jellyfin` maps each Jellyfin knob to the argument set Jellyfin would
emit under it, then calls `capabilities::required` on that argument set. The
placement rule stays the only definition of what a job needs. A new token
reaches the audit for free.

The command:

- reads `encoding.xml` with a flat tag scanner, no XML dependency, and ignores
  a tag it does not know so a newer Jellyfin key never fails the run;
- compares against the declared `capabilities` by default, because that list
  is what the scheduler reads, and against a fresh one-frame probe with
  `--probe`;
- prints one row per knob with the tokens it needs and the hosts that satisfy
  them, in four verdicts: `ok`, `partial`, `broken`, `unknown`, plus `warn` for
  the non-routing checks;
- exits 1 when a setting is `broken`, so a script can gate on it;
- never writes to `encoding.xml` and never restarts Jellyfin.

Two checks do not come from tokens. `VaapiDevice` must equal the device the
probe tested, so `capabilities` and `jellyfin` both take `--device`.
`TranscodingTempPath` must fall under a host `paths` map, checked as a static
prefix, not with IO.

The audit is stricter than placement in one place: an HEVC row asks for
`<family>_hevc` when that token exists, although `required()` returns only the
family for an 8-bit profile. ADR 0024 exists because a host can advertise the
family and fail the encoder, and the audit exists to catch that host.

## Consequences

- A setting that no worker can serve is reported before a viewer finds it, and
  `partial` names the host that cannot take the job.
- The audit inherits every gap of the probe. AV1 decode and `tonemap_opencl`
  read `unknown` rather than a guess, because the one-frame test does not
  perform them.
- The default comparison is against declared capabilities, which can be stale.
  `--probe` costs one ssh round trip per host and shows the drift.
- `encoding.xml` is read, never written. The user still edits the file and
  restarts Jellyfin; the tool prints the recommendation, it does not apply it.

## Alternatives rejected

- A web dashboard over the same data: out of scope, and Jellyfin already has a
  settings page.
- Writing `encoding.xml` back: races Jellyfin's own writer, needs a restart,
  and turns an audit into a config manager.
- Parsing Jellyfin's startup log for its own hardware detection: that detection
  runs on the client, the host without a GPU, so it answers the wrong question.
- A second table of knob-to-token rules: the drift this record exists to avoid.
