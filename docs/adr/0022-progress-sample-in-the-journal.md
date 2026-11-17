# 0022: The journal records what the job actually did

Status: Accepted
Date: 2026-11-17

## Context

ADR 0011 gave the journal its timings and a reader for them. It left out what
the job was doing. The site dashboard could show how many jobs ran, which host
took them, how long dispatch cost, and how many failed. It could not answer the
questions asked after a playback complaint: did this job keep up in realtime,
how fast did it go, what was the source, and was it a transcode at all or a
direct stream.

Jellyfin already knows the answer partly. Its own session page shows a transcode
reason. But the dispatcher is where the numbers pass by, and ffmpeg prints them
all on stderr anyway:

- `Stream #0:0: Video: hevc, ..., 3840x2160, 24 fps` under `Input #0`, then the
  same under `Output #0` with the shown codec and size.
- `Stream #0:0 -> #0:0 (copy)` or `(hevc (native) -> h264 (h264_vaapi))` under
  `Stream mapping:`, which says per stream whether it was copied or re-encoded.
- A stats line rewritten in place with `\r`, holding `fps=`, `bitrate=` and
  `speed=`, for as long as the encode runs.

Nothing reads stderr today. The SSH transport handed it to Jellyfin with
`Stdio::inherit()`, and the agent relayed the frames untouched. That is the
point of a drop-in replacement, and it is also why the numbers never reached
the journal.

## Decision

- `Event::Finished` gains a flattened `Stats`: `fps`, `speed`, `kbps`,
  `src_w`, `src_h`, `src_codec`, `src_hdr`, `dst_w`, `dst_h`, `dst_codec`, plus
  `video`, `audio` and `subtitle`, each `copy` or `encode`. Every field is
  optional and omitted when absent, so one finished line reads
  `"video":"encode","audio":"copy","fps":23.8,"speed":1.42`.
- `Event::Finished` also gains `host` and `dur_us`. The host repeats the one on
  `Started` so a dashboard counts failures per worker without joining two
  lines. `dur_us` is the transport call, the same interval `rrffmpeg-stats`
  derives from the two timestamps, because Loki cannot join them.
- `Event::Started` gains `program` (`ffmpeg` or `ffprobe`) and `in_flight`, the
  jobs already running on that host when this one started, itself included.
  Probes flood the journal and cost nothing; separating them keeps the
  latency figures honest. `in_flight` against the host's `concurrency` shows
  saturation without a second measurement pass.
- The SSH transport spawns with piped stdout and stderr and relays both to its
  own streams through a thread that copies 16 KiB chunks. The tapped stream is
  stderr, or stdout when `stderr_as_stdout` merges them on the worker. The agent
  transport taps the `STDERR` frames it already relays.
- Parsing lives in `src/progress.rs`, one process-wide slot fed by whichever
  transport runs the job and drained by `take()` when `Finished` is appended.
  A fixed slot is safe because rrffmpeg is one process per job: one job per
  process, so one slot, no key and no allocation per chunk.
- `rrffmpeg-stats` gains `speed` (p50, p05, min), `fps` (p50), a `content` tally
  by label (`2160p hevc hdr`, `1080p h264`) and a `work` tally by stream and
  verdict (`video encode`, `audio copy`). `--json` adds `speed_x1000`,
  `fps_x10`, `content` and `work`.

## Consequences

- The dispatcher now copies every byte of a transcode's output through its own
  address space. The bytes are unchanged and the order is unchanged: this is
  still a pipe to Jellyfin, not a rewriting proxy. The cost is one copy per
  chunk, measured at 107 ms for a 2 second clip on the local fallback path and
  invisible beside the encode itself.
- A sink that closes early, because Jellyfin dropped the session, stops the
  write but not the read. Draining to the end is what keeps the worker from
  blocking on a full pipe, and the job still finishes with ffmpeg's own exit
  code.
- The numbers are samples, not measurements the dispatcher controls. `speed`
  is the last stats line before exit, so a job killed halfway reports the speed
  of the frames it had done. A 0 fps line, printed before the first frame, is
  discarded rather than dragged into the median.
- A job run with `-loglevel error` or `-nostats` prints no sample and records
  no field. Jellyfin's transcodes do not pass either flag, so the gap shows up
  on probes and trickplay only, which is what one wants.
- The local fallback path (`run_local`) records nothing today, so a job that
  fell back to the client has no `Finished` line at all, and therefore no
  sample. That was already true before this record.
- The journal grows. One finished line carries roughly 120 more bytes for a
  transcoded job. At 8 MiB per file the rotation simply happens sooner.
- `Stats` is boxed inside `Event`. Ten optional fields straight in the enum
  made every `Started` in memory pay for them, which clippy flagged at 272
  bytes; the box keeps the common variant small.

## Alternatives rejected

- Emit one metrics line to a second file, as `psi.log` does. It duplicates the
  journal, and the relay would have to parse ffmpeg to fill it.
- Read the numbers from `ffprobe` before the job. It reports what the source
  is, never whether the job kept up, and it costs a second process per job.
- Have the agent daemon report samples. It sees its own side of the link, so a
  client that went away would look like a job that completed.
- Mark the streams from the arguments (`-c:v copy`, `-vn`). The mapping lines
  are ffmpeg's own verdict, and they cover `-filter_complex`, burned-in
  subtitles and default copy decisions the arguments do not state.
- Keep `Stdio::inherit()` and tap nothing in the SSH path. Then the agent
  transport would report samples and the SSH path would not, which is the
  transport split the config is not supposed to have.
