# 0024: Capabilities are declared, with one token per real limit

Status: Accepted
Date: 2026-11-18

## Context

Two workers exposed the same flat token `vaapi` with very different reality.
Measured on the same 4K HEVC 10-bit source:

| | Skylake iGPU | Kaby Lake iGPU |
|---|---|---|
| `h264_vaapi` | works | works |
| `hevc_vaapi` 8-bit | works | works |
| `hevc_vaapi` `main10` | fails | works |
| `tonemap_vaapi` | fails | works |
| 10-bit decode into a VAAPI surface | fails | works |
| `h264_qsv` | works | works |

Both boxes print `h264_vaapi`, `hevc_vaapi` and `h264_qsv` in
`ffmpeg -encoders`. A job that needs 10-bit HEVC encoding placed on the older
host waits in the queue, takes a slot and then fails, which is worse than a
clean wait.

Neither obvious machine-readable source answers the question. libva reports
`VAProfileHEVCMain10` with `VAEntrypointVLD` on the host that cannot initialise
the decode context Jellyfin asks for. `avcodec_get_hw_config` only lists the
device types and pixel formats an encoder accepts, not what the driver does
with them. Only a one-frame encode is truthful.

## Decision

Advertise sub-capabilities, all of which a job must match:

- `vaapi`, `qsv`, `nvenc`: the encoder family, as before.
- `vaapi_hevc`: HEVC encoding in hardware.
- `vaapi_hevc10`: 10-bit HEVC encoding, which implies the decode side works.
- `vaapi_tonemap`: `tonemap_vaapi` can build a filter chain.
- `vaapi_vpp`: VAAPI video processing filters, `scale_vaapi` first.

The required set comes from the arguments, never from the input file:
a `*_vaapi` encoder requires `vaapi`, `main10` or `p010` adds
`vaapi_hevc10`, a `tonemap_vaapi` filter adds `vaapi_tonemap`. An empty
required set matches any host. A host with an empty list accepts any job, as
before.

The map is written once in config, per host. `rrffmpeg capabilities [host]`
runs the one-frame probe over ssh and prints the exact TOML rows, which you
paste. Nothing probes at dispatch: placement still reads memory only.

## Consequences

- A 10-bit job waits for a host that can encode it, instead of failing on one
  that cannot. The failure moves from "after a slot was taken" to never.
- The config can go stale. A driver update that adds or removes a profile is
  not noticed until someone re-runs the probe. The cost of a stale mapping is
  one failed job, not a wrong answer at dispatch time.
- One more sub-token per real limit. A new generation adds a token, not a
  change to the scheduler.
- `hevc_vaapi` with an 8-bit profile requires only `vaapi`, so a host that
  lists `hevc_vaapi` but fails to start it still needs `vaapi_hevc` in the
  required set. That token exists for the probe output and for a manual pin.

## Alternatives rejected

- A crate over libva (`libva`, `oxideav-vaapi`) and `vainfo`: a C build
  dependency on the client, and the profile it reports is the one that lied
  in the first place.
- `avcodec_get_hw_config` through `ffmpeg-next`: answers a narrower question
  and still needs a local ffmpeg build.
- Probing on every dispatch, or caching a probe per host at startup: the
  dispatch path must stay off IO, and Jellyfin starts one process per job, so
  a per-process probe would cost one ssh round trip per job.
- Trusting `ffmpeg -encoders` on the worker: both hosts list `h264_qsv` and
  one lists capabilities its driver refuses at runtime.
