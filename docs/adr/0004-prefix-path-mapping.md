# 0004: Longest-prefix path mapping in both directions

Status: Accepted
Date: 2026-11-17

## Context

The worker mounts media at a different root than the client, and HLS segments
land on a client tmpfs reached through a second mount on the worker. rffmpeg
leaves this to a per-worker wrapper script that rewrites arguments. That script
is the reason the production pair is hard to move.

Jellyfin sends inputs as `file:/abs/path`, never as a bare path, so a rule that
only matches a leading `/` matches nothing. Outputs are written to client paths
and must be rewritten in the other direction. Media names are arbitrary bytes,
not necessarily UTF-8.

## Decision

Each host carries a list of `from` and `to` prefix pairs. Rewrite every
argument in both directions:

- `from` to `to` before the job is dispatched.
- `to` to `from` for paths that come back.

Matching works on `Path` components, never on strings, so a prefix must end on
a component boundary and `/home/a` does not match `/home/ab`. The longest
matching prefix wins, compared first by component count and then by byte
count. Ties keep the first rule in config order. A path that matches no rule
passes through unchanged.

The `file:` scheme is handled explicitly: the mapping applies inside the
prefix, including .NET's quotes around a path with spaces. Other schemes
(`http:`, `pipe:`) and plain filter expressions are untouched. `OsString`
bytes are preserved, so a non-UTF-8 filename survives.

## Consequences

- One config entry replaces one wrapper script per worker.
- Input and output are covered by the same rules, with no special case for
  HLS segments.
- A path outside the rules is forwarded unchanged. The worker then fails with
  `ENOENT`, which is visible, but the mistake is in the config, not the code.
  The host probe checks each mapped `to` root so a missing mount is caught
  earlier.
- Two rules where one `from` is a prefix of another need care. The longest
  wins, so put the narrower rule in the same host block; the order between two
  identical prefixes does not matter.
- Mapping a path back is only used in tests today. The journal records the
  client path, and Jellyfin reads the output on its own mount.

## Alternatives rejected

- String replacement on the full argument. It rewrites `-vf` expressions and
  URLs, and it mangles non-UTF-8 names.
- A wrapper script per worker. That is the status quo this project replaces.
- Mounting media at identical paths on both sides. It forces the worker to
  match the client's layout, which is not always possible.
