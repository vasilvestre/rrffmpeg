# Configuration

Read [`INSTALL.md`](../INSTALL.md) first. This page covers the two transports,
the table that picks a mode, the rules that apply once a worker is configured,
and the audit of Jellyfin's own settings.

`config.example.toml` is the full client and worker pair, with every key
commented. The client reads `$RRFFMPEG_CONFIG`, else
`$XDG_CONFIG_HOME/rffmpeg/config.toml`, else `~/.config/rffmpeg/config.toml`.

## Agent transport

SSH needs nothing on the worker. The agent removes the per-job handshake and
the per-job probe, and it answers "is this job still running" from memory. It
speaks TLS, and the client trusts exactly one certificate: the daemon's own.

Generate the pair on the worker. List every name and address a client will use
to reach it, because the certificate is checked against `agent_addr`:

```bash
install -D rrffmpeg-agent ~/.local/bin/rrffmpeg-agent
rrffmpeg-agent --init ~/.config/rrffmpeg --san worker --san 10.8.0.2
```

That writes `cert.pem` and `key.pem`, the key mode 0600. Start the daemon:

```bash
RRFFMPEG_AGENT_TOKEN=change-me rrffmpeg-agent \
    --listen 10.8.0.2:9876 \
    --cert ~/.config/rrffmpeg/cert.pem \
    --key ~/.config/rrffmpeg/key.pem
```

Copy `cert.pem` to the client. Copy it over a channel you already trust, since
it is the only thing standing between the client and an impostor. Then:

```toml
agent_token = "change-me"
agent_cert = "/home/alice/.config/rrffmpeg/worker-cert.pem"

[[hosts]]
name = "worker"
agent_addr = "worker:9876"
```

The address may differ from `name`. A host without `agent_addr` keeps using
SSH, so one config can mix both transports.

The certificate proves the daemon to the client. The token proves the client to
the daemon; it travels inside TLS, but a stolen token still works from any host
that can reach the address. Keep the listener on a private network until mTLS
exists.

## Choosing a mode and a transport

Pick by where the dispatcher runs, not by preference. The cost of SSH is one
process and one key exchange per job. The agent removes that cost and adds a
daemon on the worker.

| Where the dispatcher runs | Configuration | Reason |
|---|---|---|
| The box has the GPU | no `[[hosts]]` | Local mode: no state, no thread, no disk |
| The box has a GPU and a worker has a better one | a `[[hosts]]` entry with `local = true` | Placement sees both, so each job goes where it runs best |
| Client without a GPU, LAN, a few jobs per hour | SSH | Nothing to install, and the cost is invisible at that rate |
| Client that sends bursts of short calls | Agent, or SSH with persistence | One TLS round trip, and no ssh process per call; persistence closes most of the SSH gap |
| Link with high latency or loss | Agent | One round trip for TLS 1.3, against several for SSH key exchange and auth |
| Worker with no inbound reachability | SSH | The agent needs a listening port on the worker |
| Worker that cannot keep a daemon running | SSH | Nothing to supervise on the worker |
| Worker that must not run arbitrary argv | SSH | An agent token is remote code execution by design |
| A title only one box can mount | `reads` on that host | Placement skips a host whose declared roots cover no input |
| Client on a shared or spinning disk | SSH or agent, `state_dir` on tmpfs | One `fdatasync` on a contended disk can cost tens of milliseconds |

Rules that apply once a worker is configured:

1. Leave `state_dir` on the disk. Any local filesystem works: on an nvme disk a
   dispatch costs 0.5 to 2.3 ms more than on tmpfs, inside the 50 ms budget.
   Put it on tmpfs only behind a shared or spinning disk. `latency.md`
   gives the numbers.
2. Never let a client without a GPU quietly transcode. After `queue_wait_secs`
   without a worker slot the process runs the local binary, unless you set
   `fallback_local = false`. Keep it false on a GPU-less client: an `ffmpeg`
   job then exits 1 and logs `no worker slot after`, while an `ffprobe` still
   falls back, because a local probe is cheap and Jellyfin needs it for
   direct-play. On a shared box the fallback runs ffmpeg at the lowest priority,
   on the wrong hardware, and can trip a memory cap.
3. A per-host key belongs in `ssh_args`, not in a shell wrapper. The global
   `ssh_args` list applies to every host, and a host's own list is appended, so
   one worker can add `-i /home/alice/.ssh/worker` while the shared options are
   written once. Both are inserted after the `ControlMaster` options and before
   the host name, so persistence keeps working. `-o` options are allowed.
4. A worker that needs variables around ffmpeg gets `[hosts.env]`, not a
   wrapper script. Each value reaches the worker literally, so a space or a `$`
   survives the remote shell. The agent transport carries the same map.
5. Capabilities come from the arguments, and you write them once per host. An
   encoder names the family and the codec (`vaapi`, `qsv`, `nvenc`, plus
   `vaapi_av1`, `qsv_av1` or `nvenc_av1` for an AV1 encoder); `main10`,
   `p010` and a `tonemap_vaapi` filter add `vaapi_hevc10` and
   `vaapi_tonemap`. All of them must match, so a 10-bit or AV1 job waits for a
   worker that can encode it instead of failing on one that only lists the
   encoder. Nothing probes on dispatch: run `rrffmpeg capabilities` on
   the client to print the exact rows, and paste them. A `-version` or
   `-encoders` probe carries no capability at all.
6. Measure before you add the agent. Time `ffprobe -version` through the
   dispatcher a few hundred times. SSH keeps one connection per host open for
   `ssh_persist_secs` (300 by default), so a job opens a channel instead of
   paying a handshake: 37.9 ms p50 on loopback, against 155.6 ms with
   persistence off. If p99 is under 50 ms, SSH is enough and the daemon is
   complexity you do not need yet. `latency.md` has the runs.
7. Give every client that shares a worker the same `state_dir`, and a distinct
   `client_name`. The slot cap and the queue both live there, so a client with
   its own private `state_dir` sees a worker that is always free and a weight
   that means nothing. `client_weight` only applies while two clients wait:
   alone, a client keeps taking every slot.
8. A client that has its own GPU declares it as a local host, `local = true`,
   and placement then treats it like any other worker. Give it the tokens it
   proved and give the remote worker only what it alone can do: an N100 listed
   with `vaapi_av1` and a local host with `vaapi`, `vaapi_hevc` and
   `vaapi_hevc10` sends AV1 jobs out and keeps the rest home. A local host
   takes no `paths`, no `ssh_args` and no `*_bin`: it runs `local_ffmpeg` and
   `local_ffprobe` on the client's own paths, and `rrffmpeg capabilities <name>`
   probes it without leaving the machine. Keep local mode (no `[[hosts]]`) when
   the client never sends anything out: it needs no state and no thread.
9. A host that cannot read some media says so: `reads = ["/mnt/archive"]`,
   written in the paths that host sees. Placement maps each `-i` value for the
   candidate host before it asks, so a root written in client terms covers
   nothing. Leave `reads` out and the host accepts any input, as before.
   Ties no longer follow the order of the file: a job that needs no accelerator
   prefers a host that needs no SSH, then a higher `prefer`, then the lower
   load, then the host name.

```toml
client_name = "client"
client_weight = 400   # 1 to 1000, same scale as the kernel's IO weight
```

## Match Jellyfin's settings

```bash
rrffmpeg jellyfin                 # reads /etc/jellyfin/encoding.xml
rrffmpeg jellyfin --path ~/.config/jellyfin/config/encoding.xml --probe
```

The command compares the transcode settings with the `capabilities` of every
host. It reads the file and prints a table; it never writes to it, and it needs
no restart of Jellyfin.

Each Jellyfin knob only chooses the arguments Jellyfin emits, and the
dispatcher passes those through unchanged. So the audit turns the knobs into
argument sets, asks `capabilities::required` which tokens they need, and checks
who advertises them. The verdicts:

- `ok`: every host can serve it.
- `partial`: some can, and the detail names the ones that cannot. A job sent
  to the wrong host waits or fails after taking a slot, so the cost is
  capacity, not correctness.
- `broken`: no host can serve it. This is a playback path that cannot work,
  and the command exits 1.
- `unknown`: the one-frame probe cannot answer. AV1 decode and the OpenCL
  tone-mapping path are the real gaps.
- `warn`: not a routing question. `EncodingThreadCount=-1` on a host with
  `concurrency` above 1 asks for every core once per job.

It also checks two things the tokens cannot show. `VaapiDevice` must be the
device the probe tested, or every token describes another GPU: pass `--device`
to both commands. And `TranscodingTempPath` must fall under a `paths` map, or
the worker writes segments to a path it does not mount.

`--probe` re-runs the one-frame probe instead of reading the declared list, so
you see the drift after a driver or ffmpeg update. `--json` prints the same
rows for a script. Run it after any change to `encoding.xml`, to the config, or
to a worker's driver.

## Next

Print the capability rows for each host, then check Jellyfin against them:

```bash
rrffmpeg capabilities worker
rrffmpeg jellyfin --probe
```
