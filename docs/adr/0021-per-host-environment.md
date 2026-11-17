# 0021: Per-host environment instead of a worker wrapper

Status: Accepted
Date: 2026-11-17

## Context

The Intel N100 worker needs `OCL_ICD_VENDORS` and `LD_LIBRARY_PATH` around
`jellyfin-ffmpeg`. Config could only name a command, so the operator kept a
six-line shell script on the worker that exported both and exec'd the real
binary. That script is the last shim the dispatcher cannot express.

## Decision

Add `env`, a map of strings, on `[[hosts]]`, and `local_env` at the top level
for the local binary.

- SSH: the remote command becomes
  `mkdir -p ... && echo $$ > pid && exec env 'K=V' ... <program> <args>`.
  Each `K=V` is single-quoted as one word, so a value with a space, `$` or `:`
  arrives literally and the remote shell never expands it.
- Agent: `Hello` carries `env: Vec<String>` and `PROTOCOL` becomes 2. The daemon
  passes the list to `Command::envs`. A peer from revision 1 is refused by the
  existing revision check, so both sides need the same release.

## Consequences

- The worker wrapper script can go, and the environment is visible in the client
  config next to the host it applies to.
- The agent protocol changes meaning, so the client and the daemon must be the
  same release. That is already the rule, and the daemon says so by name.
- Values with a newline survive the quoting but are awkward in a TOML file, so
  they stay an operator mistake rather than a supported case.
- The environment applies to every job on the host, including probes. A probe
  that needs no OpenCL pays nothing for two extra words on the command line.

## Alternatives rejected

- A `--env` flag on the agent and a shell prefix on SSH only. Two shapes for one
  concept, and the local path would stay bare.
- Keeping the wrapper script and documenting it. It is the shim this change
  exists to remove.
- An environment variable on the client that the dispatcher forwards wholesale.
  It would leak the client's environment to the worker and cannot differ per
  host.
