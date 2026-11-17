# AGENTS.md: rrffmpeg

Session context for an agent working in this folder. Read `README.md` first: it
is the pitch. This file is the operational brief.

## What this is

A Rust rewrite of rffmpeg, the dispatcher that lets Jellyfin on a GPU-less host
send transcodes to a worker that has the GPU. It builds and runs; the MVP below
is mostly in place. Read the pitch for why, this file for how.

## Build and test

```bash
cargo test                       # 59 tests, including real ffmpeg 8.x fidelity
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Layout: `src/main.rs` wires the dispatch, `config.rs` reads TOML, `paths.rs`
maps client to worker roots, `transport.rs` speaks SSH, `state.rs` is the WAL
and its flusher, `scheduler.rs` places jobs, `janitor.rs` repairs state.
`config.example.toml` is the production pair.

## Ground truth: the production deployment it must replace

This project exists because of a real, running setup. Every requirement below
comes from it, not from speculation.

- **Client**: caroline, a headless box with no GPU. Jellyfin runs natively; its
  `ffmpeg` is a symlink to the dispatcher.
- **Worker**: virmire, an Intel N100 with VAAPI on `/dev/dri/renderD128`,
  `tonemap_opencl` for HDR.
- **Path mapping is mandatory**: the worker reads the media over an rclone
  mount at a different root (`/home/my666/le4/...`) than the client
  (`/home/vasilves/...`). Today this needs a per-worker wrapper script;
  rrffmpeg should do it natively.
- **Output path**: HLS segments land back on the client's tmpfs
  (`/dev/shm/jellyfin-transcode`), reached on the worker through a second
  mount. Path mapping must cover output as well as input.
- **Encoding constraints**: `AllowHevcEncoding=false` (H.264 output);
  `h264_vaapi` cannot encode 10-bit `p010`, so 4K HDR needs `hevc_vaapi`.
  rrffmpeg must not care; it passes arguments through unchanged.

## The defects to beat (measured)

- Dispatch cost 2 to 4 s per job because the state DB commit (fsync) sat
  between "decide to run" and "run ffmpeg". On tmpfs the same dispatch is
  ~64 ms. Fix: keep bookkeeping off the critical path.
- A single SSH failure marked the only GPU host `bad` forever; no retest. It
  caused a full outage and needed an external cron watchdog. Fix:
  reconciliation in-process.
- `SIGKILL` leaves orphan state rows. Fix: a janitor.
- No priority, no per-host concurrency cap, no capability detection, no path
  mapping, no retry, a per-job SSH health probe.

## What NOT to build

- No transcoding logic. rrffmpeg only dispatches args to ffmpeg on the right
  host. Video quality, remuxing, codec choices are out of scope.
- Do not design a job model from scratch. dffmpeg already has
  coordinator + worker + heartbeat + requeue. Borrow it.
- No web UI, no database beyond what the state model needs in v1.

## MVP (v1), in order

1. Drop-in `ffmpeg` and `ffprobe` replacement: exact argument passthrough,
   correct `stdin`/`stdout`/`stderr`, and rffmpeg's `stderr_as_stdout`
   special-case. Prove this first; everything else is worthless without it.
2. One worker over SSH, with client→worker path mapping.
3. Async WAL bookkeeping plus an in-process janitor (retest hosts, reap
   orphans, requeue). Host selection uses an in-memory counter so async
   writes cannot undercount.
4. Only then: priority, capability detection, agent transport.

## Success criterion

The production pair runs on rrffmpeg for a week with no manual repair and
dispatch p99 under 50 ms.

## References

- rffmpeg source: https://github.com/joshuaboniface/rffmpeg (single Python file).
- rffmpeg-go (archived): https://github.com/aleksasiriski/rffmpeg-go
- dffmpeg (the job model to borrow): https://github.com/brwyatt/dffmpeg
- The production setup and its failure modes: `~/homelab/docs/rffmpeg.md`
  and the caroline changelog.

## Style

Plain language, active voice, short sentences. No em dashes. Commit subjects
in the imperative. For Rust, follow idiomatic ownership and error handling
(`Result`, `?`, no `unwrap` on IO paths).
