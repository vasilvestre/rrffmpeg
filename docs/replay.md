# Replay harness

The harness records real Jellyfin invocations and replays them through
`rrffmpeg`. It catches the regressions no unit test sees: argument order,
quoting, stream wiring, exit codes.

## How it works

`scripts/record.sh` appends one JSON line per invocation, then runs the command
unchanged. The line holds `program`, `argv`, `cwd` and an optional `label`.

`tests/replay.rs` replays every line in `tests/fixtures/jellyfin/*.jsonl`
through the binary. `RRFFMPEG_BIN` points at a stand-in that records the
arguments it receives, writes one marker to stdout and one to stderr, then
exits with the recorded code. The test asserts four things:

- the arguments arrive unchanged and in order;
- the exit code survives;
- stdout and stderr stay apart, unless `stderr_as_stdout` is set;
- with `stderr_as_stdout`, both streams land on stdout.

`@SRC@` in an argument becomes a short synthetic file built with `lavfi` at
test time. The tests skip when `ffmpeg` is not installed. They also run once
with `RRFFMPEG_BIN` set and once through a generated config, so neither path
can rot.

## Record on the production pair

Do this on the client, in front of Jellyfin. The recorder `exec`s the real
command, so playback is not disturbed and the exit code is unchanged.

1. Install the recorder and its log directory.

```bash
sudo install -m 755 ~/projects/oss/rrffmpeg/scripts/record.sh /usr/local/bin/rrffmpeg-record
sudo install -d -o jellyfin -g jellyfin /var/log/rrffmpeg
```

2. Create one wrapper per tool Jellyfin calls. The label tells you which tool
fired.

```bash
sudo tee /usr/local/bin/jellyfin-ffmpeg-record >/dev/null <<'EOF'
#!/bin/sh
RRFFMPEG_RECORD_LABEL=ffmpeg exec /usr/local/bin/rrffmpeg-record /usr/lib/jellyfin-ffmpeg/ffmpeg "$@"
EOF
sudo tee /usr/local/bin/jellyfin-ffprobe-record >/dev/null <<'EOF'
#!/bin/sh
RRFFMPEG_RECORD_LABEL=ffprobe exec /usr/local/bin/rrffmpeg-record /usr/lib/jellyfin-ffmpeg/ffprobe "$@"
EOF
sudo chmod 755 /usr/local/bin/jellyfin-ffmpeg-record /usr/local/bin/jellyfin-ffprobe-record
```

3. Point Jellyfin at the wrappers. In `/etc/jellyfin/encoding.xml`:

```xml
<EncoderPath>/usr/local/bin/jellyfin-ffmpeg-record</EncoderPath>
<EncoderProbePath>/usr/local/bin/jellyfin-ffprobe-record</EncoderProbePath>
```

4. Set the output file in a systemd drop-in for `jellyfin.service`.

```bash
sudo systemctl edit jellyfin
```

```ini
[Service]
Environment=RRFFMPEG_RECORD=/var/log/rrffmpeg/jellyfin.jsonl
```

5. Restart Jellyfin and exercise one of each job: a playback probe, an HLS
transcode, a thumbnail scan, a remux, a seek, and one 4K HDR title.

```bash
sudo systemctl restart jellyfin
tail -f /var/log/rrffmpeg/jellyfin.jsonl
```

6. Copy the lines you want into the repo. One file per job class, one JSON
object per line, no trailing commas.

```bash
cp /var/log/rrffmpeg/jellyfin.jsonl /tmp/session.jsonl
```

Add `"exit_code"` and `"stderr_as_stdout"` when you want them. Both default to
`0` and `false`.

7. Restore the real paths. Remove the drop-in and put the original values back
in `encoding.xml`, then restart Jellyfin.

The production pair leaves `EncoderPath` unset, so Jellyfin resolves `ffmpeg`
from `PATH`. Replacing the entry point there records the same lines, takes
effect on the next job, and needs no config edit and no restart.

## Replay

```bash
cargo test --test replay
cargo test --test replay -- --nocapture
```

`--nocapture` prints the skip line when ffmpeg is missing.

## Fixtures today

Six of the eight fixtures are recordings, not hand-written shapes. A recorder
sitting on the production entry points captured them from Jellyfin 12.1.0 on
2026-11-17. Transcodes, direct streams and remuxes came from live playback;
the probe came from a metadata refresh.

Media paths are rewritten to `/home/alice/...` placeholders before the file
lands in the repo. Argument order, flags, quoting and stream wiring are exactly
as Jellyfin sent them. Nothing opens a path: the stand-in never touches media.

| File | Source | Covers |
|---|---|---|
| `01-ffprobe-json.jsonl` | recorded | probe, `-v warning -print_format json -show_streams -show_chapters -show_format -show_frames -only_first_vframe` |
| `02-hls-transcode-progress.jsonl` | recorded | HLS transcode of a 10-bit source, `h264_vaapi`, segments under `/dev/shm/jellyfin-transcode` |
| `03-thumbnail-sprite.jsonl` | hand-written | trickplay frame, `-vf thumbnail=...,scale=...`, `-frames:v 1` |
| `04-remux-copy.jsonl` | recorded | remux, `-codec:v copy` and `-codec:a copy`, `-max_muxing_queue_size` |
| `05-4k-hdr-tonemap.jsonl` | recorded | 4K HDR10 plus Dolby Vision source, `h264_vaapi` with `tonemap_opencl` |
| `06-seek-transcode.jsonl` | recorded | seek, `-ss` before `-i`, `-readrate 10` |
| `07-ffprobe-unreadable.jsonl` | hand-written | failing probe, exit code 1 |
| `08-directstream-aac.jsonl` | recorded | direct stream, `-codec:v copy` with AAC audio |

Two classes have no recording yet: trickplay extraction, because the scheduled
task ran before the recorder was in place, and a real failing probe. Those two
lines stay hand-written.

`02` carries `stderr_as_stdout` on. That is the option under test for the
transcode path, not something the production config sets.

## What this does not catch

- Encode correctness. The stand-in ignores the arguments, so a wrong filter
  chain still passes.
- GPU behaviour. No hardware filter runs, on any machine.
- Worker dispatch, path mapping, SSH or the agent, requeue, the janitor. Every
  fixture replays in local-only mode.
- Signal handling during a live transcode.
- Arguments containing a newline or a tab. `record.sh` does not escape them.
- Which invocations Jellyfin will send next. The set is a snapshot of one
  server version; a new client profile or a new Jellyfin release can add a
  shape, and only a fresh recording catches it.

Use `tests/real_ffmpeg.rs` for output fidelity and the production pair for
dispatch behaviour.

Next:

```bash
cargo test --test replay
cargo test
```
