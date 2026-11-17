# 0020: SSH arguments belong in the config

Status: Accepted
Date: 2026-11-17

## Context

The production pair reaches its worker with a dedicated key. Config offered
`ssh_bin` and nothing else, so the operator wrapped `ssh` in a shell script
that added `-i`. A wrapper is another file to install, another path to keep
true, and it hides which key a host uses.

## Decision

Add `ssh_args` at the top level and on `[[hosts]]`. Both are lists of strings,
both default to empty. The host list is appended after the global one, so one
worker adds `-i KEY` while the shared options are written once. The combined
list goes into the ssh argv after the `ControlMaster` options and before `--`
and the host operand, in `run`, `probe` and `job_alive`.

## Consequences

- The wrapper script disappears from the client.
- Persistence still works: the `ControlMaster` options are emitted first and
  cannot be replaced by the list. ssh takes the last value of a repeated `-o`,
  so an operator who writes his own `ControlMaster` there overrides persistence.
  That is documented, not prevented.
- A key path now lives in a config file. The key itself does not, and the file
  permissions stay the operator's job.
- A malformed value, for example a string instead of a list, fails with the
  existing `Error::Config`.

## Alternatives rejected

- Fields for `ssh_key` and `ssh_options`. Two more keys to document, and an
  `-o` option still needs a generic escape hatch.
- An `$RRFFMPEG_SSH_ARGS` environment variable. Hard to quote, invisible in the
  config, and every host would share it.
- Running a shell wrapper by default. It is the defect this removes.
