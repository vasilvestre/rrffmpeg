# 0010: TLS for the agent transport, one pinned certificate

Status: Accepted, amended by ADR-0018
Date: 2026-11-17

## Context

ADR 0009 shipped the agent on plaintext TCP. A token crosses the wire in
clear, and the daemon runs whatever argv the token holder sends. That is
remote code execution for anyone who can read one packet, so the transport
was only safe inside a VPN, and the documentation had to say so.

Three ways to fix it:

1. Keep plaintext and require a tunnel.
2. TLS with a public CA certificate.
3. TLS with the daemon's own self-signed certificate, pinned on the client.

Option 1 leaves the hole one networking mistake away. Option 2 needs a public
name, a CA, and renewal on every worker, which a home lab cannot assume.
Option 3 needs nothing but a file copy, and the trust decision is already per
host: the client knows exactly which daemon it wants.

The first TLS attempt also taught a lesson about the frame layer. One TLS
stream cannot be cloned, so both directions shared one stream behind a mutex.
A reader blocked in `read` holds that mutex, so the writer starves. The
symptom was measured: a test that takes 0.16 s under `strace` took 1.17 s,
9.45 s, and 14 s on repeat runs, all spent waiting for the lock. Polling the
read with a 50 ms timeout only moves the starvation, it does not remove it.

## Decision

Use `rustls` with the `ring` provider, `rcgen` for key generation, and
`rustls-pemfile` to load PEM files. No `aws-lc-rs`: it needs cmake and a C
toolchain, which the musl release build does not have.

- The daemon serves TLS only. There is no plaintext mode and no downgrade.
- The worker gets a certificate from `rrffmpeg-agent --init DIR --san NAME`,
  valid until 2126, key written mode 0600. The client copies `cert.pem` and
  sets `agent_cert` to it.
- The client trusts exactly that certificate. It builds a root store with the
  single certificate and uses the ordinary rustls verifier, so expiry and the
  name check still run. The name is the host part of `agent_addr`, so the
  certificate must list that address or name as a subject alternative name.
- A job uses two connections, one per direction. Each stream is owned by one
  thread: no lock on the stream, no polling, no starvation. The daemon answers
  the control hello with a `ready` frame, and the client opens the stdin
  connection only after it, so the job is always registered first.
- Frames are buffered per connection and parsed incrementally, because a read
  can now stop in the middle of a header.
- The stdin connection doubles as the liveness leash: when the client dies it
  closes, the daemon sends `SIGTERM`, then `SIGKILL` after two seconds.
- The token stays a shared secret. It is checked in constant time after the
  handshake.

## Consequences

- A stolen token no longer needs a stolen packet. An attacker needs the
  certificate's name to verify against, so a token alone still works on the
  same network: the certificate proves the server, not the client. mTLS would
  close that, and it is the next step if the link is not private.
- Rotation means copying a new `cert.pem` to every client. No expiry alarm
  fires before 2126, so nothing reminds you.
- Three dependencies and a static `ring` build, so every binary carries a TLS
  implementation. The size cost is not measured here.
- Two TCP connections and two handshakes per job. Measured on loopback: one
  connection costs 0.27 ms p50, and a full job over both connections adds
  0.33 ms p50 and 0.38 ms p99 over starting the same program locally. That is
  0.7 percent of the 50 ms budget. Method and raw runs: `docs/latency.md`.
- Restarting the daemon keeps its certificate, so clients keep working. Losing
  `key.pem` means generating a new pair and updating every client.
- `rcgen` is only used by `--init` and by tests, but it links into the daemon
  binary. Splitting it behind a feature is not worth a second build profile.

## Alternatives rejected

- A private CA with `rcgen` signing leaf certificates. Correct, and more work
  than one self-signed file per worker buys in a lab.
- Public CA certificates (Let's Encrypt). Needs a public name reachable from
  the client, and a renewal hook on the worker.
- Certificate pinning by fingerprint instead of a root store. That is a custom
  verifier with the `dangerous_configuration` API, and it skips the name and
  expiry checks that the standard verifier already does.
- One connection with a shared mutex, or with a polling read. Measured to
  starve writers; see Context.
- QUIC or a channel multiplexer. Bigger surface than the problem.
