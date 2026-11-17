# 0016 Verify and replace a build without guessing

Status: Accepted

## Context

The install was six commands copied from the README: download, check the
checksum, unpack, `install`, two symlinks. Nothing in the binary reported its
version, so after an update the operator could not prove which build answered
`ffmpeg` on the Jellyfin host. A tag pushed against a stale `Cargo.toml` would
publish a binary that misreports the release it came from. The release workflow
built without running the tests, so a tag on a red commit shipped. The agent
protocol carried no revision, so a new client against an old daemon failed with
a message that did not name the cause.

## Decision

- `rrffmpeg --version` prints `rrffmpeg <CARGO_PKG_VERSION>`, and
  `rrffmpeg-agent --version` does the same. The answer comes before the config
  loads, so a broken config still reports the build.
- The check answers only when the process is named `rrffmpeg`. Invoked as
  `ffmpeg` or `ffprobe`, `--version` keeps reaching the real ffmpeg, as the
  passthrough rule in ADR 0001 requires.
- The release workflow fails when the pushed tag is not `v` plus the
  `Cargo.toml` version.
- The release workflow runs `cargo test --locked` in a gate job, and the build
  jobs need it. A tag on a red commit publishes nothing.
- `scripts/install.sh` replaces the six commands. It picks the target from
  `uname -m`, checks the checksum, installs with `install` (which replaces the
  inode, so a running transcode keeps its binary), links `ffmpeg` and
  `ffprobe`, copies the previous binary to `*.prev`, and prints the version it
  installed.
- The agent `Hello` frame carries `version`, a protocol revision. A daemon
  refuses a peer that speaks another revision and says so.

## Consequences

An update is one command plus one line of output, and the output is the proof.
A rollback is a rename. The cost: the protocol revision must be bumped by hand
when a frame or a field changes meaning, and a client and a daemon from
different releases can no longer talk at all. That is the point, but it turns a
rolling update into a stop, so the worker goes first.

`--version` is the one argument the dispatcher answers instead of forwarding,
and only under its own name. Any other exception to passthrough needs the same
name guard, or Jellyfin's probes break.

The install script downloads from `main`, not from the release it belongs to,
so a fix to the script reaches the next user without a release. The tarball
also carries the script, which keeps the offline path honest.

## Alternatives rejected

- A self-update flag in the dispatcher: a binary that rewrites itself needs a
  privileged path and a rollback story, for a machine updated a few times a
  year.
- A package repository: more moving parts than one host and one operator need.
- Printing the version on `-version` too: that argument belongs to ffmpeg, and
  Jellyfin parses the answer.
- Tolerating a protocol mismatch: the failure would surface as an empty job or
  a hung pipe, which is worse than a refusal that names the two revisions.
