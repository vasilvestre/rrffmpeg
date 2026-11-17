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

A `v*` tag runs `.github/workflows/release.yml`, which publishes static
`x86_64` and `aarch64` binaries on the releases page. The install steps live
in `README.md`.

The public page is `docs/index.html`, served by GitHub Pages from the `/docs`
folder of `main`. Keep it true to the code: config keys, features and status.

Layout: `src/main.rs` wires the dispatch, `config.rs` reads TOML, `paths.rs`
maps client to worker roots, `transport.rs` picks SSH or the agent per host,
`agent.rs` is the agent protocol and daemon, `state.rs` is the WAL and its
flusher, `scheduler.rs` places jobs, `janitor.rs` repairs state.
`config.example.toml` is the production pair.

## This repository is public

Never commit a real hostname, a real user name, an absolute path taken from a
real machine, or a credential. Use the placeholders: `client`, `worker`,
`/home/alice`, `/home/bob/media`, `/home/bob/transcode`. Real values stay in
the private homelab notes. Check before every push: `git grep -n "/home/"`
must only show `alice` and `bob`, and no `name = ` under `[[hosts]]` may name
a real machine.

## Ground truth: the production deployment it must replace

This project exists because of a real, running setup. Every requirement below
comes from it, not from speculation. The names and paths below are
placeholders, not the real ones.

- **Client** (`client`): a headless box with no GPU. Jellyfin runs natively; its
  `ffmpeg` is a symlink to the dispatcher.
- **Worker** (`worker`): an Intel N100 with VAAPI on `/dev/dri/renderD128`,
  `tonemap_opencl` for HDR.
- **Path mapping is mandatory**: the worker reads the media from a mount at a
  different root (`/home/bob/media/...`) than the client (`/home/alice/...`).
  Today this needs a per-worker wrapper script; rrffmpeg must do it natively.
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

## Decision records

Every decision that shapes the code gets a record in `docs/adr/`. The index is
`docs/adr/README.md`; read the records before you change a design choice.

- Write one file per decision, numbered `NNNN-short-title.md`.
- Use four sections: Context, Decision, Consequences, Alternatives rejected.
  State the cost of the choice, not only its benefit.
- Date the record. All records use `Status: Accepted`.
- Supersede a decision with a new record. Do not rewrite an accepted one: set
  its status to `Superseded by ADR-NNNN` and link the new one.
- Add a row to the index table in the same change.
- No real host name, user name, or absolute path from a real machine. The
  public repository rule applies to records too.
- Record the measure behind a claim (`2 to 4 s`, `64 ms`), or say it is an
  assumption.

## Deployment facts that break dispatchers

- Jellyfin derives ffprobe from the ffmpeg path: `/usr/local/bin/ffmpeg` gives
  `/usr/local/bin/ffprobe`. Install both names as symlinks to this binary. Do
  not point Jellyfin at `rrffmpeg`, it would look for `rrffprobe`.
- Media inputs arrive as `file:/abs/path`, never as a bare path. Path mapping
  handles the prefix; a rule that only matches a leading `/` maps nothing.
- Jellyfin writes `q`, `p`, `u`, `c` to stdin, and some of those have no
  newline. stdio must be a plain pipe, never a pty. The SSH transport uses
  `-T` for that reason.
- Progress comes from ffmpeg's stderr stats lines. Never merge stderr into
  stdout for a transcode, and never prefix lines.
- Startup probes (`-version`, `-encoders`, `-hwaccels`, `-h ...`) are parsed
  from stdout and run through the dispatcher too, so they must reach the
  worker and come back on stdout unchanged.

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
  and its changelog.
- Why the code looks like this: `docs/adr/` (decision records and index).

## Style

Plain language, active voice, short sentences. No em dashes. Commit subjects
in the imperative. For Rust, follow idiomatic ownership and error handling
(`Result`, `?`, no `unwrap` on IO paths).
