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

## Replay

```bash
cargo test --test replay
cargo test --test replay -- --nocapture
```

`--nocapture` prints the skip line when ffmpeg is missing.

## Fixtures today

| File | Covers |
|---|---|
| `01-ffprobe-json.jsonl` | ffprobe JSON probe, `-show_streams -show_format -show_chapters` |
| `02-hls-transcode-progress.jsonl` | HLS transcode, `-progress pipe:1`, segments under `/dev/shm/jellyfin-transcode`, `stderr_as_stdout` on |
| `03-thumbnail-sprite.jsonl` | trickplay frame, `-vf thumbnail=...,scale=...`, `-frames:v 1` |
| `04-remux-copy.jsonl` | remux, `-c copy`, `-max_muxing_queue_size` |
| `05-4k-hdr-tonemap.jsonl` | 4K HDR, `tonemap_opencl` plus `hevc_vaapi` |
| `06-seek-error-loglevel.jsonl` | seek, `-ss` before `-i`, `-t`, `-loglevel error` |
| `07-ffprobe-unreadable.jsonl` | failing probe, exit code 1 |

The paths are real Jellyfin shapes. Nothing opens them: the stand-in never
touches the media.

## What this does not catch

- Encode correctness. The stand-in ignores the arguments, so a wrong filter
  chain still passes.
- GPU behaviour. No hardware filter runs, on any machine.
- Worker dispatch, path mapping, SSH, requeue, the janitor. Every fixture
  replays in local-only mode.
- Signal handling during a live transcode.
- Arguments containing a newline or a tab. `record.sh` does not escape them.

Use `tests/real_ffmpeg.rs` for output fidelity and the production pair for
dispatch behaviour.

Next:

```bash
cargo test --test replay
cargo test
```
