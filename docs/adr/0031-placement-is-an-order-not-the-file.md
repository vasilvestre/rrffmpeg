# 0031: Placement is an order, not the order of the file

Status: Accepted

Date: 2026-11-19

## Context

ADR 0006 broke a placement tie with config order. That made the file a routing
rule: two operators with the same two hosts got different placements, and the
only way to move a class of job was to edit and reload the config. The
failing install in ADR 0030 shows the cost: the fix it needed was to reorder
the `[[hosts]]` blocks, which is not a statement about the hosts at all.

Placement also had no way to say "this host is the better one when both can run
the job", and no way to say "do not open ssh for a software encode".

## Decision

A host declares its standing, and the comparison is a total order:

1. When the job needs no accelerator token, a host that needs no ssh wins.
   `local = true` and an `agent_addr` both need no ssh. A software encode gains
   nothing from a GPU, so it stays on the client instead of paying a handshake
   and a network round trip.
2. A higher `prefer` integer wins. The default is 0 for every host, so an
   undeclared preference is a tie.
3. A lower `running / concurrency` ratio wins, compared by cross-multiplying
   integers as before. This is the load balance ADR 0006 introduced.
4. The host name wins the last tie, in byte order. Duplicate names are refused
   by the config, so this always decides.

Declaration order is not read anywhere in `Scheduler`. Swapping two `[[hosts]]`
blocks changes nothing, which is the property the tests pin.

`prefer` is a tie-break, not a filter: it never puts a job on a host that lacks
a token, cannot read the input (ADR 0030), is down, or is full. Use it for the
standing facts that token lists cannot express, for example "the archive box is
the one with the fast disk" or "the N100 is the one to fill first".

## Consequences

- Routing is reproducible from the file contents alone. Two installs with the
  same hosts, in any order, place the same job on the same host.
- A software encode on a client that has a local host now runs locally even
  when a worker is idle. That is the point, and it is the one behaviour change
  for a config that declares neither `prefer` nor `reads`. A config with no
  local host and no `agent_addr` keeps every placement it had, because the
  first rule then ties and the load ratio still decides.
- A job that needs a token uses the accelerator rule only when the host can
  serve it, so `prefer` and the ratio order the rest. A GPU worker that needs
  ssh is not penalised for the handshake, because the job needs the GPU.
- The name tie-break is stable but arbitrary. An operator who wants a
  particular host first among equals writes `prefer`, not a name.
- The ssh rule reads the host fields, not the transport the router would pick.
  A host with both `agent_addr` and an ssh fallback still counts as direct,
  because the agent path is the one the transport takes.

## Alternatives rejected

- **Keep config order as the last tie-break.** It is the bug: the same two
  hosts place differently depending on how the file was written.
- **A string rank or a priority list per host.** One integer answers "which
  host first" without inventing a second place to express capability.
- **Prefer the local host for every job.** A transcode that needs a token the
  client lacks would then queue behind a host that can never take it.
- **Prefer the fastest host by measured throughput.** It needs per-job timing
  in the decision path, and it turns a placement rule into a statistic that
  changes under load. ADR 0006 rejected that class of measurement on cost.
