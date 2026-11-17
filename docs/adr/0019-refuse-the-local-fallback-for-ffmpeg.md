# 0019: Refuse the local fallback for a transcode

Status: Accepted
Date: 2026-11-17

## Context

The production client is a shared seedbox with Jellyfin and no GPU. It has one
worker over SSH. Until now, a job that found no free slot after
`queue_wait_secs` ran the local `ffmpeg`, and nothing in the config could stop
it. On that client the local `ffmpeg` is software encoding at the lowest
priority, on the wrong hardware, inside the disk and memory budget of other
tenants. The README listed this as a risk and told the operator to watch the
log.

A probe is different: Jellyfin asks for direct-play decisions, the answer is
cheap, and a local answer is better than a failed request.

## Decision

Add `fallback_local`, default `true`, so a v0.5.0 config behaves as before.
When it is `false` and the program is `ffmpeg`, a `Dispatch::NoSlot` logs one
error line naming the wait and exits 1. The same rule applies in local-only
mode, where a missing config would otherwise encode on the client. When the
program is `ffprobe` the local binary still runs, whatever the switch says.

## Consequences

- A saturated worker produces a failed transcode, not a silent software encode.
  The playback fails fast and the journal still records the job attempt.
- An operator who upgrades and forgets the new key keeps the old risk. The
  default is deliberately unchanged: compatibility beats safety by default here,
  and the README says to set the key on a GPU-less client.
- A local `ffprobe` answers with the client's build, which may differ from the
  worker's. That was already true and only affects metadata, not output.

## Alternatives rejected

- Refuse the fallback for both programs. Jellyfin would lose direct-play
  decisions while the worker is busy, and the probe costs nothing.
- Make `false` the default. Every existing one-box install would start failing
  transcodes after an upgrade.
- Return a different exit code. Jellyfin treats any non-zero as a failed job,
  and 1 matches what the local binary returns on its own errors.

## Measure

The failure path costs no dispatch: it is the existing wait, then a log line
and `exit 1`. The existing retry and queue tests are unchanged.
