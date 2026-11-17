# 0023. The sample is a peak, not a tail

Status: Accepted
Date: 2026-11-17

## Context

ADR 0022 records `fps`, `speed` and the bitrate on the finished line, parsed
from the stats lines ffmpeg rewrites in place. Version 0.7.0 kept the last
value it parsed.

Two measurements on a real worker showed that value is wrong for the question
people ask it. Transcoding a 1080p source to H.264 with VAAPI and the exact
arguments Jellyfin sends, the speed field read 1.92x after two seconds and
5.04x after a minute, then fell to 3.5x at the end. The player, reading a line
in the middle of the job, reported 5.76x.

The cause is that `fps` and `speed` are running averages since the job
started, not momentary rates. A slow start pulls them down for as long as the
job is short, and any stall pulls them down again: the frame count stops while
the clock runs. A first sample of a job that lasted five seconds therefore read
`fps: 5.4, speed: 2.12`, while the same job peaked near 5x.

A second defect hid the first. ffmpeg pads these values to a fixed width
(`fps= 85`, `speed=   5x`, `bitrate= 900.0kbits/s`), so splitting the line on
spaces produced an empty value for the padded keys and the parser silently kept
the earlier number.

## Decision

`fps` and `speed` are the highest running average the job reached. The parser
reads the number that follows the key, past the padding, and keeps the maximum
over the job.

The bitrate keeps the last value, because it is a moment and not a total.

## Consequences

The recorded speed matches what the player showed live, and the dashboard
percentiles mean "how fast this job ran at its best", not "how the job ended".

A job that stalls at the end and never recovers keeps the higher number. The
figure is a throughput record, not evidence of a healthy job, so pair it with
the exit code and `dur_us`.

`fps` and `speed` stay proportional to the source frame rate, which is what
makes them comparable across jobs.

A job that dies before its first frame records `fps: 0.0` and no speed.
`rrffmpeg-stats` skips a zero frame rate when it takes the median.

## Alternatives rejected

- **Keep the last line.** It answers "how did this job end", which is not the
  question a dashboard asks, and it reports a slow start as a slow machine.
- **Record every stats line.** A long job prints one line a second, so the
  journal would grow by tens of kilobytes per job and Loki would index noise.
  The 8 MiB rotation exists to keep the journal small.
- **Compute the rate ourselves from the frame counter over time.** The same
  information with more code, and it cannot tell a slow encoder from a stalled
  one any better than the peak does.
- **Ask the player.** Jellyfin's own figure is what ADR 0022 wanted to stop
  depending on: it is not in the journal and not per job.
