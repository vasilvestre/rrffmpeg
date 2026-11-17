# Install rrffmpeg

This file covers a real install on a Jellyfin host with a separate GPU worker.
`README.md` explains why the project exists. `config.example.toml` is the full
list of keys.

Two roles appear below:

- **client**: the host where Jellyfin runs. It has no GPU, and its `ffmpeg` and
  `ffprobe` become symlinks to `rrffmpeg`.
- **worker**: the host with the GPU. It runs plain `ffmpeg`, over SSH or through
  `rrffmpeg-agent`.

## 1. Prerequisites

On the client:

- Jellyfin, running as the user that owns the media.
- A real `ffmpeg` and `ffprobe` binary, outside the PATH entry you will replace.
  For a distribution package that is `/usr/bin/ffmpeg` and `/usr/bin/ffprobe`.
- A local filesystem for `state_dir`: ext4, xfs, btrfs, tmpfs. Not NFS, not
  CIFS. See step 5.

On the worker:

- `ffmpeg` on the PATH of the SSH user, or of the agent user.
- Read access to the media at the mapped path.
- Write access to the output directory, if HLS segments land there.
- For a hardware encoder, read and write access to the GPU device.

Check the GPU before you write the config:

```bash
ls -l /dev/dri/                       # renderD128 is the VAAPI device
vainfo --display drm --device /dev/dri/renderD128 | head
ffmpeg -hide_banner -encoders | grep -E 'vaapi|nvenc|qsv|amf'
```

`nvidia-smi` covers NVENC instead of `/dev/dri`. If no device and no encoder
appears, the worker cannot take a hardware job, and the install stops there.

## 2. Install the binary

Both hosts get the same static binary. A `v*` tag publishes `x86_64` and
`aarch64` builds for musl, with a `.sha256` file.

```bash
curl -LO https://github.com/vasilvestre/rrfmpeg/releases/latest/download/rrffmpeg-x86_64-unknown-linux-musl.tar.gz
sha256sum -c rrffmpeg-x86_64-unknown-linux-musl.tar.gz.sha256
tar xzf rrffmpeg-x86_64-unknown-linux-musl.tar.gz
sudo install -m 755 rrffmpeg-x86_64-unknown-linux-musl/rrffmpeg /usr/local/bin/rrffmpeg
sudo install -m 755 rrffmpeg-x86_64-unknown-linux-musl/rrffmpeg-stats /usr/local/bin/rrffmpeg-stats
```

The agent path also needs `rrffmpeg-agent` on the worker. Install it the same
way, then continue with step 4.

## 3. Replace ffmpeg on the client

```bash
sudo ln -sf /usr/local/bin/rrffmpeg /usr/local/bin/ffmpeg
sudo ln -sf /usr/local/bin/rrffmpeg /usr/local/bin/ffprobe
```

Both names matter: Jellyfin finds `ffprobe` next to `ffmpeg`, so a missing
`ffprobe` link breaks every probe.

The client now runs `rrffmpeg` whenever Jellyfin calls `ffmpeg`. With no config
file, `rrffmpeg` runs the local `ffmpeg` and changes nothing. That local binary
must not resolve back to `rrffmpeg`, or the exec loops:

```toml
local_ffmpeg = "/usr/bin/ffmpeg"
local_ffprobe = "/usr/bin/ffprobe"
```

Set these explicitly in the config. The default is the bare name `ffmpeg`,
which resolves through the PATH you just replaced.

## 4. Write the config

The client reads `$RRFFMPEG_CONFIG`, else
`$XDG_CONFIG_HOME/rffmpeg/config.toml`, else `~/.config/rffmpeg/config.toml`.
This minimal file sends every job to one worker over SSH:

```toml
state_dir = "/home/alice/.local/state/rrffmpeg"
local_ffmpeg = "/usr/bin/ffmpeg"
local_ffprobe = "/usr/bin/ffprobe"

[[hosts]]
name = "worker"
concurrency = 2
capabilities = ["vaapi"]

[[hosts.paths]]
from = "/home/alice"
to = "/home/bob/media"

[[hosts.paths]]
from = "/dev/shm/jellyfin-transcode"
to = "/home/bob/transcode"
```

- `name` is the SSH destination: `worker`, `user@worker`, or an address.
- `concurrency` is the number of jobs the worker accepts at once.
- `capabilities` lists the accelerators the worker really has. Empty accepts any
  job, which is wrong for a GPU-less worker.
- `[[hosts.paths]]` rewrites the input and the output roots. The longest
  matching `from` wins, in both directions.

Use SSH keys for the user that runs Jellyfin, so a transcode never waits on a
password prompt:

```bash
sudo -u jellyfin ssh-keygen -t ed25519 -N '' -f ~jellyfin/.ssh/id_ed25519
sudo -u jellyfin ssh-copy-id worker
```

Anything that reads stdin matters: `rrffmpeg` passes `q`, `p`, `u` and `c`
through a plain pipe, never a pty.

### Agent transport instead of SSH

Do this only when SSH dispatch p95 gets close to the 50 ms budget. The daemon
trades a per-job handshake for a listening port.

On the worker:

```bash
install -D rrffmpeg-agent ~/.local/bin/rrffmpeg-agent
rrffmpeg-agent --init ~/.config/rrffmpeg --san worker --san 10.8.0.2
```

`--san` must list every name and address a client uses in `agent_addr`, because
the certificate is checked against it. Then start the daemon under a supervisor,
with the token in the environment and not on the command line:

```ini
# /etc/systemd/system/rrffmpeg-agent.service
[Unit]
Description=rrffmpeg agent
After=network-online.target

[Service]
User=bob
Environment=RRFFMPEG_AGENT_TOKEN=change-me
ExecStart=/home/bob/.local/bin/rrffmpeg-agent \
    --listen 10.8.0.2:9876 \
    --cert /home/bob/.config/rrffmpeg/cert.pem \
    --key /home/bob/.config/rrffmpeg/key.pem
Restart=always

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now rrffmpeg-agent
```

Copy `cert.pem` to the client over a channel you already trust. It is the only
thing that proves the daemon's identity. Then, on the client:

```toml
agent_token = "change-me"
agent_cert = "/home/alice/.config/rrffmpeg/worker-cert.pem"

[[hosts]]
name = "worker"
agent_addr = "worker:9876"
```

Keep the listener on a private network. The token proves the client to the
daemon, so anyone who steals it can run arbitrary commands on the worker, as
that user.

## 5. Place `state_dir`

`state_dir` holds the journal, the pid files and the slot files that cap
concurrency. Every client that shares a worker must point at the same directory,
or the cap and the queue mean nothing.

`state_dir` must be a local filesystem. On a network mount the slot files fail
open without an error, and the concurrency cap silently stops working.

```bash
findmnt -T /home/alice/.local/state/rrffmpeg -o TARGET,FSTYPE,SOURCE
```

Disk choice per `docs/latency.md`: an nvme disk costs 0.5 to 2.3 ms per
dispatch more than tmpfs, which fits inside the budget. Use tmpfs only when the
disk is shared or spinning:

```bash
sudo mkdir -p /dev/shm/rrffmpeg
# in config: state_dir = "/dev/shm/rrffmpeg"
```

tmpfs loses the journal at reboot. That is harmless: the journal exists for the
janitor, not for history.

The directory must be writable by the Jellyfin user. `rrffmpeg` also creates the
SSH control socket directory and refuses one it does not own and that is not a
plain directory, so keep both under the same user.

## 6. Verify

Start with passthrough, which proves the transport and the stream wiring:

```bash
ffmpeg -version
ffprobe -version
```

Both must print the worker's ffmpeg banner on stdout and exit 0. Then a real
transcode, which also proves the path mapping:

```bash
ffmpeg -hide_banner -i /home/alice/media/film.mkv -t 10 \
    -c:v h264_vaapi -f null -
```

Then read the timings back:

```bash
rrffmpeg-stats
```

`dispatch` gives the cost `rrffmpeg` adds. `host worker` gives the failure
count. A `down` count above zero means the worker refused a job; check the SSH
login or the agent port.

At `RRFFMPEG_LOG=debug` the client prints one line per job. Keep the default
`warn` in production.

## 7. For AI agents

An agent can run this install, but it must not assume the hardware. Run the
checks, print the output, and ask the user when the answer is not in the output.

### Ask before you assume

Ask the user, in one message, when any of these is unknown:

1. Which host has the GPU, and whether it is Intel VAAPI, Nvidia NVENC, or AMD.
2. Whether the worker mounts the media at the same path as the client, and at
   which root.
3. Whether `state_dir` sits on the same disk as the media library.
4. Whether two Jellyfin installs share one worker.
5. Whether the user accepts a daemon and a listening port on the worker, or
   wants SSH only.

Never invent a `capabilities` value to make a job place. A wrong value sends a
transcode to a host that cannot encode it, and the failure appears later as a
transcode error, not at dispatch.

### Check GPU availability

On the worker, in this order. Stop at the first that matches and record the
result:

```bash
# Intel / AMD VAAPI
ls -l /dev/dri/ && vainfo | head -20
# Nvidia
nvidia-smi --query-gpu=name,driver_version --format=csv
# What ffmpeg can actually use, which is what rrffmpeg dispatches to
ffmpeg -hide_banner -hwaccels
ffmpeg -hide_banner -encoders | grep -E 'vaapi|nvenc|qsv|amf|videotoolbox'
```

Decision table:

| Output | `capabilities` |
|---|---|
| `h264_vaapi` or `hevc_vaapi` listed, `/dev/dri/renderD128` present | `["vaapi"]` |
| `h264_nvenc` or `hevc_nvenc` listed, `nvidia-smi` works | `["nvenc"]` |
| `h264_qsv` listed | `["qsv"]` |
| Several of the above | list all of them |
| No hardware encoder, only `libx264` | no `[[hosts]]` at all, or leave `capabilities` empty and accept CPU encoding |

`rrffmpeg` derives the needed capability from the encoder flag Jellyfin passes:
`-c:v hevc_vaapi` needs `vaapi`, `-c:v h264_nvenc` needs `nvenc`, and so on. So
the list must cover the encoders Jellyfin will ask for, not only the one in the
test command.

Check the permissions of the user that will run the job:

```bash
id -nG            # must include `render` for /dev/dri, `video` for some setups
test -r /dev/dri/renderD128 && test -w /dev/dri/renderD128 && echo ok
```

If the device is missing or `vainfo` fails, report the output and ask the user.
Do not fall back to a CPU transcode without saying so: on a 4K HEVC source that
is a realtime failure, not a slow start.

### Check disk contention

The bookkeeping must never wait on a busy disk. Measure before placing
`state_dir`:

```bash
findmnt -T /home/alice/.local/state -o TARGET,FSTYPE,SOURCE
cat /proc/pressure/io              # some avg10, full avg10
iostat -x 1 5                     # %util and await per device
```

Read them like this:

- `FSTYPE` is `nfs`, `cifs` or `fuse.*`: stop. `state_dir` must move to a local
  filesystem, or the concurrency cap silently stops working.
- `some avg10` in `/proc/pressure/io` above about 10, or `%util` above about 80
  on the device holding `state_dir`: the disk is contended. Put `state_dir` on
  tmpfs (`/dev/shm/rrffmpeg`) and say why in the report.
- Otherwise keep `state_dir` on the disk. It survives a reboot, which is what
  the janitor wants.

Also check the space and the rotation:

```bash
du -sh /home/alice/.local/state/rrffmpeg
ls /home/alice/.local/state/rrffmpeg
```

The journal rotates at 8 MiB per file and keeps the rotated files, so the
directory only grows. A few hundred MB free is enough.

### Check the path mapping

On the worker, with the same user that runs the job:

```bash
ls -l /home/bob/media                    # the `to` root of the input mapping
touch /home/bob/transcode/.probe && rm /home/bob/transcode/.probe
```

Jellyfin passes inputs as `file:/home/alice/...`, never as a bare path.
`rrffmpeg` maps the prefix, so the `from` value must match the real string,
`/home/alice`, and not a subdirectory of it.

The janitor probes every `to` root. A missing mount marks the host down instead
of failing each job one by one, so a wrong `to` path shows up as a down host in
`rrffmpeg-stats`, not as an ENOENT in the log.

### What an agent must not do

- Do not edit `~/.config/rffmpeg/config.toml` on the client without printing the
  resulting file to the user.
- Do not remove the `ffmpeg` symlink to "test" the install without putting it
  back in the same command.
- Do not set `agent_addr` before the daemon answers `systemctl status
  rrffmpeg-agent`.
- Do not copy a private key, or the agent token, into a file in this repository.
- Do not run a 4K transcode to "check" the worker on a machine the user needs.
  A 10 second `-t 10 -f null -` is enough.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `ffmpeg -version` prints twice or hangs | `local_ffmpeg` resolves to the symlink | set the real binary path |
| Every job fails with ENOENT | `paths.from` does not match the input prefix | use the client root, not a subdirectory |
| No worker takes the job, then it runs slowly on the client | capability mismatch or a down host | check `rrffmpeg-stats`, then the `capabilities` list |
| `no worker slot after 600s` in the log | all slots busy, or a stale down verdict | raise `queue_wait_secs`, or wait `down_ttl_secs` |
| Transcode works, output is empty | the output path is not mapped | add a `[[hosts.paths]]` for the HLS directory |

## Next

```bash
rrffmpeg-stats
journalctl -u rrffmpeg-agent --since -10min
```
