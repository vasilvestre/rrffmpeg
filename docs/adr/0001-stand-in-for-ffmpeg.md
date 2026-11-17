# 0001: Stand in for ffmpeg and ffprobe, pass arguments unchanged

Status: Accepted
Date: 2026-11-17

## Context

Jellyfin calls `ffmpeg` by absolute path, derives `ffprobe` from that path, and
expects the tool to behave like ffmpeg: same arguments, same streams, same exit
code, same startup probes. A dispatcher that needs Jellyfin changes beyond
`EncoderPath` would be adopted by nobody.

rffmpeg works this way: one entry point that rffmpeg installs in place of
ffmpeg. That constraint is the contract. Everything else is a scheduler
decision around it.

## Decision

Ship one binary, installed as both `ffmpeg` and `ffprobe` symlinks. Never name
it `rrffmpeg` in front of Jellyfin, because Jellyfin would look for
`rrffprobe`.

Pick the tool in this order:

1. `RRFFMPEG_PROGRAM` when set.
2. The basename of `argv[0]` when it contains `probe`.
3. The argument shape, when the install uses one name for both: any of
   `-print_format`, `-show_streams`, `-show_format`, `-show_chapters`,
   `-show_frames`, `-show_entries`, `-select_streams`, `-show_packets` means
   ffprobe.

With no worker configured, `exec` the local binary with the arguments
untouched. Rewrite nothing in the arguments except mapped paths.

## Consequences

- Startup probes (`-version`, `-encoders`, `-hwaccels`) go through the
  dispatcher and come back on stdout unchanged, as long as a worker is up.
- Local-only mode is a true `exec`: no thread, no journal, no state, no
  measurable overhead.
- Jellyfin's `stderr_as_stdout` quirk is reproduced with `dup2` on stderr
  before the exec.
- An argument that is not valid UTF-8 cannot cross SSH. The transport refuses
  the job with an explicit error instead of corrupting it.
- `-c:v h264_vaapi` and friends never reach the dispatcher: the arguments pass
  through. No video logic lives here.

## Alternatives rejected

- A wrapper script on the client. Someone has to maintain the quoting, and
  Jellyfin's probes still pay the SSH cost.
- A separate `rrffmpeg` command that Jellyfin points at with a second config
  file. More setup, same result, and Jellyfin still derives ffprobe.
