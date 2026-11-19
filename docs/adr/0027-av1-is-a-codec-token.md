# 0027: AV1 is a codec token, not a profile flag

Status: Accepted
Date: 2026-11-19

## Context

ADR 0024 gave one token per real limit, but every token described H.264 out
and HEVC out. AV1 was invisible. An `av1_vaapi` encoder ends in `vaapi`, so
`required()` returned the bare family token and every VAAPI host became
eligible. A host whose GPU has no AV1 encoder takes the job, takes a slot, and
fails to initialise the encoder.

The gap is not theoretical. Hardware AV1 encoding exists only from Intel Arc
(Alchemist) and Meteor Lake onward. A UHD 630 (Gen9.5) and the UHD Graphics of
an N100 (Alder Lake-N, Gen12) decode AV1 but cannot encode it. Both would
advertise `vaapi` and both would be picked for an `av1_vaapi` job.

AV1 is not a profile of HEVC either, so `main10` reasoning does not apply: an
AV1 encoder needs its own capability, and the HEVC tokens say nothing about
it.

## Decision

- Add three tokens: `vaapi_av1`, `qsv_av1`, `nvenc_av1`.
- The one-frame probe in `capabilities.rs` encodes one AV1 frame per family
  and prints the token when it succeeds, like every other token.
- `required()` adds `<family>_av1` when the encoder argument starts with
  `av1_`, and skips the HEVC and tone-map tokens for that job: an AV1 encoder
  is a codec choice, not a profile of HEVC.
- Software AV1 (`libsvtav1`) still requires nothing, so a host with no
  accelerator can run it.

## Consequences

- An AV1 job waits for a host that proved it encodes AV1, or falls back per
  the configured policy, instead of failing after taking a slot.
- On the current fleet no host encodes AV1, so an `av1_vaapi` job finds no
  eligible worker. That is the truthful answer: routing cannot invent an
  encoder the silicon does not have. Hardware AV1 needs Arc or newer.
- AV1 *decode* is not covered. Jellyfin names the input codec only in the
  file, not in the arguments, and ADR 0024 rejects probing the input on the
  dispatch path. A job that only decodes AV1 still matches any `vaapi` host.
- The probe grows by three bounded encodes, still under the 30 s per-test
  timeout and still one line.

## Alternatives rejected

- Treat AV1 as part of `vaapi`, i.e. probe nothing: keeps the current wrong
  placement, where a job fails after a slot is taken.
- Add a single `av1` token independent of the family: a host could list it
  while only one of `av1_qsv`, `av1_nvenc` or `av1_vaapi` works, and the
  scheduler cannot tell which encoder the job will call.
- Detect AV1 from the input file: needs an ffprobe on the dispatch path, which
  the latency budget forbids.
