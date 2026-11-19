//! How a job reaches a worker. SSH is the zero-install transport; an agent
//! transport comes later behind the same trait.
//!
//! `run` builds one remote shell command string and hands it to `ssh` as a
//! single argument. Client arguments never touch a local shell: each one is
//! single-quoted, so spaces, `$`, backticks, semicolons and newlines reach the
//! worker as literal bytes.

use crate::config::Program;
use crate::{Error, Host, Job, Result};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

pub trait Transport: Send + Sync {
    /// Run `job` on `host` with this process's stdio already wired.
    /// Returns the exit code the remote ffmpeg produced.
    fn run(&self, host: &Host, job: &Job, stderr_as_stdout: bool) -> Result<i32>;

    /// Cheap liveness check: the host answers, and every mapped root is
    /// readable. A dead mount fails here, so it must not be called once per job.
    fn probe(&self, host: &Host) -> bool;

    /// Is the remote process for `job_id` still running?
    fn job_alive(&self, host: &Host, job_id: &str) -> bool;
}

pub struct Ssh {
    pub ssh_bin: PathBuf,
    /// Remote directory for per-job pid files.
    pid_dir: String,
    timeout: Duration,
    /// Keep one ssh connection per host open between jobs, so a job pays a
    /// channel open instead of a full handshake. `None` spawns a fresh ssh.
    pub persist: Option<Persist>,
    /// Extra ssh arguments for every host, before each host's own.
    pub ssh_args: Vec<String>,
}

/// `ControlMaster` settings. `dir` must exist; ssh creates the socket in it.
#[derive(Debug, Clone)]
pub struct Persist {
    pub dir: PathBuf,
    pub secs: u64,
}

impl Ssh {
    pub fn new(ssh_bin: PathBuf) -> Ssh {
        Ssh {
            ssh_bin,
            pid_dir: "/tmp/rrffmpeg".into(),
            timeout: Duration::from_secs(5),
            persist: None,
            ssh_args: Vec::new(),
        }
    }

    /// Whole-second connect timeout, never zero: ssh rejects 0 there.
    fn connect_timeout(&self) -> String {
        self.timeout.as_secs().max(1).to_string()
    }

    fn persist_args(&self) -> Vec<String> {
        let Some(p) = &self.persist else {
            return Vec::new();
        };
        vec![
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}/%C", p.dir.display()),
            "-o".into(),
            format!("ControlPersist={}", p.secs),
        ]
    }

    /// Options that belong after the `ControlMaster` set and before the host.
    /// A host appends to the global list, so its key stays local while the
    /// shared options are written once.
    fn ssh_args(&self, host: &Host) -> Vec<String> {
        self.ssh_args
            .iter()
            .chain(&host.ssh_args)
            .cloned()
            .collect()
    }

    /// Runs [`crate::capabilities::script`] on the worker and returns the
    /// tokens it printed. A probe, not a job: no pid file, no journal row.
    /// `device` must be the render node Jellyfin uses.
    pub fn probe_capabilities(&self, host: &Host, device: &str) -> Result<Vec<String>> {
        let ffmpeg = host.ffmpeg_bin.as_deref().unwrap_or("ffmpeg");
        let out = Command::new(&self.ssh_bin)
            .arg("-T")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout()))
            .args(self.persist_args())
            .args(self.ssh_args(host))
            .arg("--")
            .arg(&host.name)
            .arg(crate::capabilities::script(ffmpeg, device))
            .output()
            .map_err(|e| Error::Io(e, format!("probing {}", host.name)))?;
        if !out.status.success() {
            return Err(Error::Transport(format!(
                "capability probe on {} failed: {}",
                host.name,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(crate::capabilities::parse(&String::from_utf8_lossy(
            &out.stdout,
        )))
    }

    /// `mkdir -p ... && echo $$ > <pid> && exec <program> <args> [2>&1]`.
    ///
    /// `$$` is not quoted: the remote shell expands it before `exec` replaces
    /// the shell with ffmpeg, so the pid in the file is the ffmpeg pid.
    fn remote_command(&self, job: &Job, host: &Host, stderr_as_stdout: bool) -> Result<String> {
        let program = match job.program {
            crate::Program::Ffmpeg => host.ffmpeg_bin.as_deref().unwrap_or("ffmpeg"),
            crate::Program::Ffprobe => host.ffprobe_bin.as_deref().unwrap_or("ffprobe"),
        };
        let mut words: Vec<&str> = Vec::with_capacity(job.args.len() + 1);
        words.push(program);
        for arg in &job.args {
            // A remote command is a shell string, so an argument that is not
            // valid UTF-8 cannot be carried without corrupting it.
            let arg = arg.to_str().ok_or_else(|| {
                Error::Transport(format!("job {} has a non-UTF-8 argument", job.id))
            })?;
            words.push(arg);
        }
        let command: Vec<String> = words.iter().map(|w| shell_quote(w)).collect();
        // `env` needs each assignment as one literal word: the remote shell
        // must not expand a `$` or split a value that contains a space.
        let env: Vec<String> = host
            .env
            .iter()
            .map(|(k, v)| shell_quote(&format!("{k}={v}")))
            .collect();
        let cmd = if env.is_empty() {
            command.join(" ")
        } else {
            format!("env {} {}", env.join(" "), command.join(" "))
        };

        let pid_file = format!("{}/{}.pid", self.pid_dir, job.id);
        // A finished job leaves its pid file behind: removing it needs a second
        // ssh round trip, and a tiny file in tmpfs is cheaper than that.
        // shortcut: pid files accumulate in pid_dir until the box reboots,
        // prune them when that becomes a problem.
        Ok(format!(
            "mkdir -p {dir} && echo $$ > {pid} && exec {cmd}{redirect}",
            dir = shell_quote(&self.pid_dir),
            pid = shell_quote(&pid_file),
            cmd = cmd,
            redirect = if stderr_as_stdout { " 2>&1" } else { "" },
        ))
    }
}

/// Quote one word for a POSIX shell with single quotes. Inside single quotes
/// every byte is literal; the only escape needed is for a single quote, which
/// closes the quote, emits an escaped quote, and reopens it.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What the probe runs on the worker. Without a root to check the host only
/// has to answer. Otherwise every mapped root and every declared `reads` root
/// must stat within 5 s, so a dropped FUSE mount marks the host down instead of
/// failing each job with ENOENT, and a hung mount cannot stall the janitor
/// cycle.
///
/// shortcut: only the root itself is checked, so an empty directory left by a
/// mount that never came up still passes. Add a per-host sentinel path when
/// that bites.
fn probe_command(host: &Host) -> String {
    let mut checks: Vec<String> = Vec::new();
    for root in host.paths.iter().map(|p| &p.to).chain(&host.reads) {
        let check = format!(
            "timeout 5 ls -d {} >/dev/null",
            shell_quote(&root.to_string_lossy())
        );
        // A reads root that repeats a path target needs no second stat.
        if !checks.contains(&check) {
            checks.push(check);
        }
    }
    if checks.is_empty() {
        return "true".into();
    }
    checks.join(" && ")
}

/// Copy one stream to this process's own, byte for byte and as it arrives,
/// and feed the progress parser: ffmpeg writes the job's fps and speed on
/// stderr, so the only way to keep them is to read the stream that `inherit`
/// used to hand straight to Jellyfin.
fn relay(
    mut src: impl Read + Send + 'static,
    mut dst: impl Write + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    crate::progress::feed(&buf[..n]);
                    if dst.write_all(&buf[..n]).is_ok() {
                        let _ = dst.flush();
                    }
                    // A closed sink (Jellyfin went away) must not stop the
                    // drain: the worker would block on a full pipe.
                }
            }
        }
    })
}

impl Transport for Ssh {
    fn run(&self, host: &Host, job: &Job, stderr_as_stdout: bool) -> Result<i32> {
        let remote = self.remote_command(job, host, stderr_as_stdout)?;
        let mut child = Command::new(&self.ssh_bin)
            .arg("-T")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .args(self.persist_args())
            .args(self.ssh_args(host))
            // `--` closes the option list: the destination comes next, then
            // the remote command. Putting it after the host would make ssh
            // pass a literal `--` as the first word of the remote command.
            .arg("--")
            .arg(&host.name)
            .arg(&remote)
            .stdin(Stdio::inherit())
            // Only the tapped stream is piped. The other inherits this
            // process's fd, so the kernel copies it and no relay thread runs.
            .stdout(if stderr_as_stdout {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stderr(if stderr_as_stdout {
                Stdio::inherit()
            } else {
                Stdio::piped()
            })
            .spawn()
            .map_err(|e| Error::Io(e, format!("running {}", self.ssh_bin.display())))?;
        // With `stderr_as_stdout` the worker merges its stderr into stdout, so
        // the tapped stream is the other one.
        let relay = if stderr_as_stdout {
            child.stdout.take().map(|out| relay(out, std::io::stdout()))
        } else {
            child.stderr.take().map(|err| relay(err, std::io::stderr()))
        };
        let status = child
            .wait()
            .map_err(|e| Error::Io(e, format!("waiting on {}", self.ssh_bin.display())))?;
        if let Some(relay) = relay {
            let _ = relay.join();
        }
        // Exit code 255 is ssh's own failure (unreachable host, dropped link),
        // not ffmpeg's. A signal-terminated ssh has no code either. Either way
        // this is a transport failure, and the journal must not record it as a
        // completed job.
        match status.code() {
            Some(255) | None => Err(Error::Transport(format!(
                "ssh to {} failed (exit {:?})",
                host.name,
                status.code()
            ))),
            Some(code) => Ok(code),
        }
    }

    fn probe(&self, host: &Host) -> bool {
        Command::new(&self.ssh_bin)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout()))
            .args(self.persist_args())
            .args(self.ssh_args(host))
            .arg("--")
            .arg(&host.name)
            .arg(probe_command(host))
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn job_alive(&self, host: &Host, job_id: &str) -> bool {
        let pid_file = format!("{}/{}.pid", self.pid_dir, job_id);
        // A missing pid file makes `cat` fail, the substitution empty, and
        // `kill -0 ""` fail, so a lost job reads as dead.
        let check = format!("kill -0 \"$(cat {})\"", shell_quote(&pid_file));
        Command::new(&self.ssh_bin)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg(format!("ConnectTimeout={}", self.connect_timeout()))
            .args(self.persist_args())
            .args(self.ssh_args(host))
            .arg("--")
            .arg(&host.name)
            .arg(&check)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }
}

/// The client's own machine as a worker. The job runs here through the same
/// relay as over ssh, so Jellyfin gets the progress lines either way, and the
/// scheduler places it like any other host.
#[derive(Debug)]
pub struct Local {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Per-job pid files, on this machine. The janitor reads them like the
    /// worker's, so a job left by a killed dispatcher is still reaped.
    pub pid_dir: PathBuf,
    /// `local_env`; a host's own `env` goes on top.
    pub env: std::collections::BTreeMap<String, String>,
}

impl Local {
    /// Refuses a local tool that is this program. `local_ffmpeg` is often a
    /// symlink next to the Jellyfin one, and pointing it at rrffmpeg would
    /// make every local job call itself, forking until the box dies.
    pub fn new(
        ffmpeg: PathBuf,
        ffprobe: PathBuf,
        pid_dir: PathBuf,
        env: std::collections::BTreeMap<String, String>,
    ) -> Result<Local> {
        for bin in [&ffmpeg, &ffprobe] {
            if resolves_to_this_program(bin) {
                return Err(Error::Config(format!(
                    "{} is this program: point local_ffmpeg and local_ffprobe at the real ffmpeg, or a host with `local = true` would call itself forever",
                    bin.display()
                )));
            }
        }
        Ok(Local {
            ffmpeg,
            ffprobe,
            pid_dir,
            env,
        })
    }

    fn binary(&self, program: Program) -> &PathBuf {
        match program {
            Program::Ffmpeg => &self.ffmpeg,
            Program::Ffprobe => &self.ffprobe,
        }
    }

    fn pid_file(&self, job_id: &str) -> PathBuf {
        self.pid_dir.join(format!("{job_id}.pid"))
    }

    /// The same one-frame probe as the ssh transport, run here.
    pub fn probe_capabilities(&self, device: &str) -> Result<Vec<String>> {
        let script = crate::capabilities::script(&self.ffmpeg.to_string_lossy(), device);
        let out = Command::new("sh")
            .arg("-c")
            .arg(&script)
            .envs(&self.env)
            .output()
            .map_err(|e| Error::Io(e, "running the capability probe".into()))?;
        if !out.status.success() {
            return Err(Error::Transport(format!(
                "capability probe on this host failed: {}",
                out.status
            )));
        }
        Ok(crate::capabilities::parse(&String::from_utf8_lossy(
            &out.stdout,
        )))
    }
}

impl Transport for Local {
    fn run(&self, host: &Host, job: &Job, stderr_as_stdout: bool) -> Result<i32> {
        let bin = self.binary(job.program);
        let mut command = Command::new(bin);
        command
            .args(&job.args)
            .envs(&self.env)
            .envs(&host.env)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            // Same wiring as ssh: only the tapped stream is piped.
            .stderr(Stdio::piped());
        // Jellyfin cancels a transcode by killing the process it started, which
        // is this one. Without this, the ffmpeg child outlives the dispatcher,
        // keeps writing into the same output directory and holds the GPU.
        // Local mode never had that problem: it execs in place.
        // SAFETY: the closure runs in the forked child before exec, where only
        // async-signal-safe calls are allowed. prctl is one, and it returns a
        // value this ignores rather than touching any shared state.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| Error::Io(e, format!("running {}", bin.display())))?;

        // Written before the job can be forgotten. The janitor reaps a job
        // whose pid file is missing, so a job that runs without one looks dead
        // while it still encodes. A kill keeps that from happening.
        let pid_file = self.pid_file(&job.id);
        if let Err(e) = std::fs::create_dir_all(&self.pid_dir)
            .and_then(|()| std::fs::write(&pid_file, child.id().to_string()))
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Io(e, format!("pid file {}", pid_file.display())));
        }

        let relay = child.stderr.take().map(|err| {
            if stderr_as_stdout {
                relay(err, std::io::stdout())
            } else {
                relay(err, std::io::stderr())
            }
        });
        let status = child.wait();
        if let Some(relay) = relay {
            let _ = relay.join();
        }
        let _ = std::fs::remove_file(&pid_file);
        let status = status.map_err(|e| Error::Io(e, format!("waiting on {}", bin.display())))?;
        // A signal-terminated child has no code; report it the way a shell
        // does, 128 plus the signal, instead of pretending it was a clean 0.
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
    }

    fn probe(&self, host: &Host) -> bool {
        // The same env as a job: a wrapper that needs it would otherwise mark
        // a working host down.
        Command::new(&self.ffmpeg)
            .arg("-version")
            .envs(&self.env)
            .envs(&host.env)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn job_alive(&self, _host: &Host, job_id: &str) -> bool {
        let Ok(text) = std::fs::read_to_string(self.pid_file(job_id)) else {
            // No pid file: the job is gone, or never wrote one.
            return false;
        };
        let Ok(pid) = text.trim().parse::<i32>() else {
            return false;
        };
        // Signal 0 asks the kernel whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }
}

/// Does this command resolve to the running program? `local_ffmpeg` is a bare
/// name in most configs, so a PATH search is what catches the symlink case.
fn resolves_to_this_program(bin: &std::path::Path) -> bool {
    let Ok(me) = std::env::current_exe() else {
        return false;
    };
    let candidate = if bin.components().count() > 1 {
        Some(bin.to_path_buf())
    } else {
        // What execvp picks: the first entry that is an executable file. Plain
        // `exists` would stop on a file the kernel refuses, and the job would
        // then reach the real one, which may be this program.
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(bin))
                .find(|path| is_executable(path))
        })
    };
    candidate.is_some_and(|path| same_file(&path, &me))
}

fn is_executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Identity by inode, so a hardlink or a bind mount of this program counts too.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Picks a transport per host: `local` runs here, `agent_addr` means the
/// daemon, else ssh.
pub struct Router {
    pub ssh: Ssh,
    pub agent: Option<crate::agent::Agent>,
    /// Built only when a host declares `local = true`.
    pub local: Option<Local>,
}

impl Router {
    fn agent(&self, host: &Host) -> Result<&crate::agent::Agent> {
        self.agent.as_ref().ok_or_else(|| {
            Error::Config(format!(
                "host {} uses agent_addr but no agent_token is set",
                host.name
            ))
        })
    }

    fn local_transport(&self, host: &Host) -> Result<&Local> {
        self.local.as_ref().ok_or_else(|| {
            Error::Config(format!(
                "host {} is local but no local transport was built",
                host.name
            ))
        })
    }
}

impl Transport for Router {
    fn run(&self, host: &Host, job: &Job, stderr_as_stdout: bool) -> Result<i32> {
        if host.local {
            return self.local_transport(host)?.run(host, job, stderr_as_stdout);
        }
        match host.agent_addr {
            Some(_) => self.agent(host)?.run(host, job, stderr_as_stdout),
            None => self.ssh.run(host, job, stderr_as_stdout),
        }
    }

    fn probe(&self, host: &Host) -> bool {
        if host.local {
            return self
                .local_transport(host)
                .map(|local| local.probe(host))
                .unwrap_or(false);
        }
        match host.agent_addr {
            Some(_) => self.agent(host).map(|a| a.probe(host)).unwrap_or(false),
            None => self.ssh.probe(host),
        }
    }

    fn job_alive(&self, host: &Host, job_id: &str) -> bool {
        if host.local {
            return self
                .local_transport(host)
                .map(|local| local.job_alive(host, job_id))
                .unwrap_or(false);
        }
        match host.agent_addr {
            Some(_) => self
                .agent(host)
                .map(|a| a.job_alive(host, job_id))
                .unwrap_or(false),
            None => self.ssh.job_alive(host, job_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PathMap, Program};
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn fixture(name: &str) -> PathBuf {
        // The process id keeps two concurrent test binaries, for example two
        // checkouts being tested at once, out of each other's directory.
        let dir = std::env::temp_dir()
            .join(format!("rrffmpeg-transport-{}", std::process::id()))
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write script");
        let mut perm = std::fs::metadata(path).expect("stat script").permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).expect("chmod script");
    }

    const RECORD: &str = "#!/bin/sh\n{ for a in \"$@\"; do printf '%s\\0' \"$a\"; done; } > ";

    /// A fake ssh that records its argv, then runs the remote command locally
    /// with a `bin` directory in front of PATH, so quoting is tested for real.
    fn fake_exec_ssh(dir: &Path, record: &Path, bin: &Path) -> PathBuf {
        let path = dir.join("ssh");
        let body = format!(
            "{RECORD}'{rec}'\nlast=\nfor a in \"$@\"; do last=$a; done\n\
             PATH='{bin}':\"$PATH\"\nexec sh -c \"$last\"\n",
            rec = record.display(),
            bin = bin.display(),
        );
        executable(&path, &body);
        path
    }

    /// A fake ssh that records its argv and exits with a fixed code.
    fn fake_exit_ssh(dir: &Path, record: &Path, code: i32) -> PathBuf {
        let path = dir.join("ssh");
        let body = format!("{RECORD}'{}'\nexit {code}\n", record.display());
        executable(&path, &body);
        path
    }

    fn recorded(path: &Path) -> Vec<String> {
        let bytes = std::fs::read(path).expect("recorded argv");
        let mut parts: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
        if parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
        parts
            .into_iter()
            .map(|p| String::from_utf8(p.to_vec()).expect("utf-8 argument"))
            .collect()
    }

    fn host() -> Host {
        Host {
            local: false,
            name: "worker".into(),
            concurrency: 1,
            capabilities: vec![],
            ffmpeg_bin: None,
            ffprobe_bin: None,
            agent_addr: None,
            ssh_args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            paths: vec![],
            reads: Vec::new(),
            prefer: 0,
        }
    }

    fn ssh(bin: PathBuf, dir: &Path) -> Ssh {
        Ssh {
            ssh_bin: bin,
            pid_dir: dir
                .join("pid")
                .to_str()
                .expect("utf-8 temp path")
                .to_string(),
            timeout: Duration::from_secs(5),
            persist: None,
            ssh_args: Vec::new(),
        }
    }

    fn job(args: &[&str]) -> Job {
        Job {
            id: "job1".into(),
            program: Program::Ffmpeg,
            args: args.iter().map(OsString::from).collect(),
            priority: 0,
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn shell_quote_wraps_and_escapes_only_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(
            shell_quote("$HOME `id` ; rm -rf /"),
            "'$HOME `id` ; rm -rf /'"
        );
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_handles_newline_and_nul() {
        // A newline is a literal byte inside single quotes.
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
        // A NUL is valid UTF-8, so the quote layer keeps it as-is. An exec
        // argument cannot carry a NUL, so spawning with one is refused rather
        // than silently truncated.
        assert_eq!(shell_quote("a\0b"), "'a\0b'");
        let refused = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("a\0b")
            .status();
        assert!(refused.is_err(), "a NUL argument must be refused");
    }

    #[test]
    fn remote_command_has_the_exact_shape() {
        let dir = fixture("shape");
        let _serial = crate::exclusive();
        let s = ssh(dir.join("unused-ssh"), &dir);
        let remote = s
            .remote_command(&job(&["a b", "it's", "$HOME", "x;y"]), &host(), false)
            .expect("remote command");
        let pid_dir = dir.join("pid");
        let pid_dir = pid_dir.display();
        assert_eq!(
            remote,
            format!(
                "mkdir -p '{pid_dir}' && echo $$ > '{pid_dir}/job1.pid' \
                 && exec 'ffmpeg' 'a b' 'it'\\''s' '$HOME' 'x;y'"
            )
        );
    }

    #[test]
    fn a_host_can_override_the_remote_command() {
        let dir = fixture("override");
        let _serial = crate::exclusive();
        let s = ssh(dir.join("unused-ssh"), &dir);
        let mut h = host();
        h.ffmpeg_bin = Some("/home/bob/.bin/rrj/jellyfin-ffmpeg.sh".into());
        let remote = s.remote_command(&job(&["-version"]), &h, false).unwrap();
        assert!(remote.ends_with("exec '/home/bob/.bin/rrj/jellyfin-ffmpeg.sh' '-version'"));
        // ffprobe keeps its own default.
        let mut j = job(&["-version"]);
        j.program = Program::Ffprobe;
        assert!(
            s.remote_command(&j, &h, false)
                .unwrap()
                .contains("exec 'ffprobe'")
        );
    }

    #[test]
    fn the_redirect_is_added_only_when_asked() {
        let dir = fixture("redirect");
        let _serial = crate::exclusive();
        let s = ssh(dir.join("unused-ssh"), &dir);
        let plain = s
            .remote_command(&job(&["-v", "error"]), &host(), false)
            .unwrap();
        let merged = s
            .remote_command(&job(&["-v", "error"]), &host(), true)
            .unwrap();
        assert!(!plain.contains("2>&1"));
        assert!(merged.ends_with(" 2>&1"));
        assert!(merged.starts_with("mkdir -p "));
    }

    #[test]
    fn ssh_gets_one_remote_argument_and_no_local_shell() {
        let dir = fixture("argv");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("ffmpeg"), "#!/bin/sh\nexit 42\n");
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);

        let code = s.run(&host(), &job(&[]), false).expect("run");
        assert_eq!(code, 42);

        let v = recorded(&argv);
        assert_eq!(v[0], "-T");
        assert_eq!(v[1], "-o");
        assert_eq!(v[2], "ServerAliveInterval=15");
        assert_eq!(v[3], "--");
        assert_eq!(v[4], "worker");
        assert_eq!(v.len(), 6);
        assert!(v[5].starts_with("mkdir -p "));
        assert!(v[5].contains("job1.pid"));
    }

    #[test]
    fn persistence_is_three_ssh_options_before_the_host() {
        let dir = fixture("persist");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("ffmpeg"), "#!/bin/sh\nexit 0\n");
        let mut s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        s.persist = Some(Persist {
            dir: dir.join("sock"),
            secs: 300,
        });

        s.run(&host(), &job(&[]), false).expect("run");

        let v = recorded(&argv);
        let at = v
            .iter()
            .position(|a| a == "ControlMaster=auto")
            .expect("ControlMaster option");
        assert_eq!(v[at - 1], "-o");
        assert_eq!(v[at + 1], "-o");
        assert!(v[at + 2].starts_with("ControlPath="));
        assert_eq!(v[at + 3], "-o");
        assert_eq!(v[at + 4], "ControlPersist=300");
        let host_at = v.iter().position(|a| a == "--").expect("--");
        assert!(at < host_at, "options must precede the host");

        let probe_argv = dir.join("probe-argv");
        let mut probing = ssh(fake_exit_ssh(&dir, &probe_argv, 0), &dir);
        probing.persist = s.persist.clone();
        assert!(probing.probe(&host()));
        assert!(
            recorded(&probe_argv)
                .iter()
                .any(|a| a == "ControlMaster=auto")
        );
    }

    #[test]
    fn configured_ssh_args_come_between_persistence_and_the_host() {
        let dir = fixture("ssh-args");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("ffmpeg"), "#!/bin/sh\nexit 0\n");
        let mut s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        s.ssh_args = vec!["-o".into(), "StrictHostKeyChecking=yes".into()];
        s.persist = Some(Persist {
            dir: dir.join("sock"),
            secs: 300,
        });
        let mut h = host();
        h.ssh_args = vec!["-i".into(), "/home/alice/.ssh/worker".into()];

        s.run(&h, &job(&[]), false).expect("run");

        let v = recorded(&argv);
        let persist = v
            .iter()
            .position(|a| a == "ControlPersist=300")
            .expect("ControlPersist");
        assert_eq!(v[persist + 1], "-o");
        assert_eq!(v[persist + 2], "StrictHostKeyChecking=yes");
        assert_eq!(v[persist + 3], "-i");
        assert_eq!(v[persist + 4], "/home/alice/.ssh/worker");
        assert_eq!(v[persist + 5], "--");
        assert_eq!(v[persist + 6], "worker");
    }

    #[test]
    fn configured_ssh_args_need_no_persistence() {
        let dir = fixture("ssh-args-nopersist");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("ffmpeg"), "#!/bin/sh\nexit 0\n");
        let mut s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        s.ssh_args = vec!["-i".into(), "key".into()];

        s.run(&host(), &job(&[]), false).expect("run");

        let v = recorded(&argv);
        assert_eq!(v[0], "-T");
        assert_eq!(v[1], "-o");
        assert_eq!(v[2], "ServerAliveInterval=15");
        assert_eq!(v[3], "-i");
        assert_eq!(v[4], "key");
        assert_eq!(v[5], "--");
        assert_eq!(v[6], "worker");
        assert!(!v.iter().any(|a| a.starts_with("Control")));
    }

    #[test]
    fn host_env_prefixes_the_command_and_survives_the_shell() {
        let dir = fixture("env");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let out = dir.join("out");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(
            &bin.join("ffmpeg"),
            &format!(
                "#!/bin/sh\nprintf '%s|%s' \"$A\" \"$B\" > '{}'\n",
                out.display()
            ),
        );
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        let mut h = host();
        h.env.insert("A".into(), "a b $HOME".into());
        h.env.insert("B".into(), "x:y".into());

        s.run(&h, &job(&[]), false).expect("run");

        assert_eq!(
            std::fs::read_to_string(&out).expect("env reached the child"),
            "a b $HOME|x:y"
        );
        let remote = s.remote_command(&job(&[]), &h, false).unwrap();
        assert!(
            remote.ends_with("&& exec env 'A=a b $HOME' 'B=x:y' 'ffmpeg'"),
            "{remote}"
        );
    }

    #[test]
    fn hostile_arguments_arrive_unchanged_on_the_worker() {
        let dir = fixture("hostile");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let out = dir.join("out");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(
            &bin.join("ffmpeg"),
            &format!("{RECORD}'{}'\nexit 0\n", out.display()),
        );
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);

        let args = ["a b", "it's", "$HOME", "x;y", "`id`", "line\nbreak", "-c:v"];
        let code = s.run(&host(), &job(&args), false).expect("run");
        assert_eq!(code, 0);
        assert_eq!(recorded(&out), args);
    }

    #[test]
    fn the_pid_file_holds_the_remote_pid() {
        let dir = fixture("pidfile");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("ffmpeg"), "#!/bin/sh\nexit 0\n");
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);

        s.run(&host(), &job(&[]), false).expect("run");
        let text = std::fs::read_to_string(Path::new(&s.pid_dir).join("job1.pid"))
            .expect("pid file exists");
        let pid: u32 = text.trim().parse().expect("pid is a number");
        assert!(pid > 0);
    }

    #[test]
    fn a_failing_transfer_marks_the_probe_down() {
        let dir = fixture("probe-down");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let s = ssh(fake_exit_ssh(&dir, &argv, 1), &dir);
        assert!(!s.probe(&host()));
        let v = recorded(&argv);
        assert_eq!(
            v,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "--",
                "worker",
                "true"
            ]
        );
    }

    #[test]
    fn a_reachable_host_passes_the_probe() {
        let dir = fixture("probe-up");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        assert!(s.probe(&host()));
    }

    #[test]
    fn the_probe_checks_every_mapped_root() {
        let dir = fixture("probe-paths");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let s = ssh(fake_exit_ssh(&dir, &argv, 0), &dir);
        let mut h = host();
        h.paths = vec![
            PathMap {
                from: "/home/alice".into(),
                to: "/home/bob/media".into(),
            },
            PathMap {
                from: "/dev/shm/jellyfin-transcode".into(),
                to: "/home/bob/transcode".into(),
            },
        ];

        assert!(s.probe(&h));
        let v = recorded(&argv);
        assert_eq!(
            v[6],
            "timeout 5 ls -d '/home/bob/media' >/dev/null && \
             timeout 5 ls -d '/home/bob/transcode' >/dev/null"
        );
    }

    /// A host can declare a root it reads without mapping it, and a mount that
    /// drops there must mark the host down the same way a path target does.
    #[test]
    fn the_probe_checks_a_declared_reads_root_without_a_path_map() {
        let dir = fixture("probe-reads");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let s = ssh(fake_exit_ssh(&dir, &argv, 0), &dir);
        let mut h = host();
        h.reads = vec!["/mnt/archive".into()];
        assert!(s.probe(&h));
        let v = recorded(&argv);
        assert_eq!(v[6], "timeout 5 ls -d '/mnt/archive' >/dev/null");

        // Repeated by a path target, the root is stat'ed once.
        h.paths = vec![PathMap {
            from: "/archive".into(),
            to: "/mnt/archive".into(),
        }];
        assert!(s.probe(&h));
        assert_eq!(
            recorded(&argv)[6],
            "timeout 5 ls -d '/mnt/archive' >/dev/null"
        );
    }

    #[test]
    fn a_missing_mapped_root_marks_the_host_down() {
        let dir = fixture("probe-no-root");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        let mut h = host();
        h.paths = vec![PathMap {
            from: "/home/alice".into(),
            to: dir.join("unmounted").to_str().expect("utf-8 path").into(),
        }];

        assert!(!s.probe(&h));
    }

    #[test]
    fn a_live_pid_means_the_job_is_alive() {
        let dir = fixture("alive");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        let pid_dir = Path::new(&s.pid_dir).to_path_buf();
        std::fs::create_dir_all(&pid_dir).unwrap();
        std::fs::write(pid_dir.join("job1.pid"), std::process::id().to_string()).unwrap();

        assert!(s.job_alive(&host(), "job1"));
        let v = recorded(&argv);
        assert_eq!(v[1], "BatchMode=yes");
        assert!(v[6].starts_with("kill -0 \"$(cat '"));
        assert!(v[6].contains("/job1.pid"));
    }

    #[test]
    fn a_missing_pid_file_means_the_job_is_dead() {
        let dir = fixture("dead");
        let _serial = crate::exclusive();
        let argv = dir.join("argv");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let s = ssh(fake_exec_ssh(&dir, &argv, &bin), &dir);
        assert!(!s.job_alive(&host(), "job1"));
    }

    #[test]
    fn a_non_utf8_argument_is_refused_instead_of_corrupted() {
        let dir = fixture("non-utf8");
        let _serial = crate::exclusive();
        let s = ssh(dir.join("unused-ssh"), &dir);
        use std::os::unix::ffi::OsStringExt;
        let mut job = job(&["-i"]);
        job.args.push(OsString::from_vec(b"bad\xff".to_vec()));

        assert!(s.remote_command(&job, &host(), false).is_err());
    }

    #[test]
    fn router_prefers_the_agent_when_the_host_has_one() {
        let dir = fixture("router");
        let _serial = crate::exclusive();
        let router = Router {
            ssh: ssh(dir.join("unused-ssh"), &dir),
            agent: None,
            local: None,
        };

        // No agent is configured. A host with agent_addr still takes the agent
        // path and reports the missing token, instead of falling back to ssh.
        let mut with_agent = host();
        with_agent.agent_addr = Some("agent.invalid:9876".into());
        let err = router
            .run(&with_agent, &job(&[]), false)
            .expect_err("agent path chosen");
        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(!router.probe(&with_agent));

        // The same router sends a host without agent_addr over ssh.
        let argv = dir.join("ssh-argv");
        let ssh_router = Router {
            ssh: ssh(fake_exit_ssh(&dir, &argv, 0), &dir),
            agent: None,
            local: None,
        };
        assert_eq!(
            ssh_router.run(&host(), &job(&[]), false).expect("ssh path"),
            0
        );
    }

    #[test]
    fn router_has_a_local_arm() {
        let dir = fixture("router-local");
        let _serial = crate::exclusive();
        let marker = dir.join("marker");
        let bin = dir.join("local-ffmpeg");
        executable(
            &bin,
            &format!("#!/bin/sh\necho ran \"$@\" > '{}'\n", marker.display()),
        );
        let local = Local::new(
            bin,
            dir.join("local-ffprobe"),
            dir.join("pid"),
            std::collections::BTreeMap::new(),
        )
        .expect("local transport");
        let router = Router {
            ssh: ssh(dir.join("unused-ssh"), &dir),
            agent: None,
            local: Some(local),
        };
        let mut here = host();
        here.local = true;

        assert_eq!(
            router
                .run(&here, &job(&["-i", "movie.mkv"]), false)
                .unwrap(),
            0
        );
        let recorded = std::fs::read_to_string(&marker).expect("the local ffmpeg ran");
        assert_eq!(recorded.trim(), "ran -i movie.mkv");
        // The probe answers for the binary, and the pid file is gone again.
        let pid_file = dir.join("pid").join("job1.pid");
        assert!(!pid_file.exists());
    }

    #[test]
    fn a_local_job_gets_local_env_then_the_host_env() {
        let dir = fixture("local-env");
        let _serial = crate::exclusive();
        let out = dir.join("env");
        let bin = dir.join("local-ffmpeg");
        executable(
            &bin,
            &format!(
                "#!/bin/sh\nprintf '%s|%s' \"$FOO\" \"$BAR\" > '{}'\n",
                out.display()
            ),
        );
        let mut env = std::collections::BTreeMap::new();
        env.insert("FOO".to_string(), "a".to_string());
        env.insert("BAR".to_string(), "b".to_string());
        let local = Local::new(bin, dir.join("p"), dir.join("pid"), env).expect("local");
        let mut here = host();
        here.local = true;
        here.env.insert("BAR".to_string(), "c".to_string());

        local.run(&here, &job(&[]), false).expect("local run");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "a|c");
    }

    #[test]
    fn a_signal_terminated_local_job_reports_128_plus_the_signal() {
        let dir = fixture("local-signal");
        let _serial = crate::exclusive();
        let bin = dir.join("local-ffmpeg");
        executable(&bin, "#!/bin/sh\nkill -TERM $$\n");
        let local =
            Local::new(bin, dir.join("p"), dir.join("pid"), Default::default()).expect("local");
        assert_eq!(local.run(&host(), &job(&[]), false).unwrap(), 143);
    }

    #[test]
    fn a_local_pid_file_says_whether_the_job_still_runs() {
        let dir = fixture("local-alive");
        let _serial = crate::exclusive();
        let local = Local::new(
            dir.join("local-ffmpeg"),
            dir.join("p"),
            dir.join("pid"),
            Default::default(),
        )
        .expect("local");
        std::fs::create_dir_all(dir.join("pid")).expect("pid dir");

        assert!(!local.job_alive(&host(), "missing"));

        std::fs::write(
            dir.join("pid").join("mine.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        assert!(local.job_alive(&host(), "mine"));

        // A pid the kernel cannot have handed out.
        std::fs::write(dir.join("pid").join("gone.pid"), "2147483647").unwrap();
        assert!(!local.job_alive(&host(), "gone"));

        std::fs::write(dir.join("pid").join("junk.pid"), "not a pid").unwrap();
        assert!(!local.job_alive(&host(), "junk"));
    }

    #[test]
    fn a_local_tool_that_is_this_program_is_refused() {
        let _serial = crate::exclusive();
        let me = std::env::current_exe().expect("current exe");
        let err = Local::new(
            me.clone(),
            PathBuf::from("ffprobe"),
            PathBuf::from("pid"),
            Default::default(),
        )
        .expect_err("this program must be refused");
        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(format!("{err}").contains("call itself"));
    }

    #[test]
    fn the_local_capability_probe_parses_the_script_output() {
        let dir = fixture("local-caps");
        let _serial = crate::exclusive();
        let bin = dir.join("local-ffmpeg");
        // The script runs the binary once per case, so printing one token per
        // run still gives each token once after the filter.
        executable(
            &bin,
            "#!/bin/sh\ncase \"$*\" in *h264_vaapi*) exit 0;; *) exit 1;; esac\n",
        );
        let local =
            Local::new(bin, dir.join("p"), dir.join("pid"), Default::default()).expect("local");
        let tokens = local
            .probe_capabilities(crate::capabilities::DEFAULT_VAAPI_DEVICE)
            .expect("probe");
        // The probe prints a token when the encoder's exit status is 0, so a
        // fake that only accepts h264_vaapi proves the parse keeps order and
        // drops nothing it should have seen.
        assert_eq!(tokens, vec!["vaapi".to_string(), "vaapi_vpp".to_string()]);
    }

    #[test]
    fn a_local_probe_without_the_binary_reports_no_token() {
        let dir = fixture("local-missing");
        let _serial = crate::exclusive();
        let local = Local::new(
            dir.join("no-such-ffmpeg"),
            dir.join("p"),
            dir.join("pid"),
            Default::default(),
        )
        .expect("local");
        assert!(!local.probe(&host()));
        // The script ends with `true`, so a missing binary yields no token
        // instead of a failing probe: the same answer ssh gives.
        assert!(
            local
                .probe_capabilities(crate::capabilities::DEFAULT_VAAPI_DEVICE)
                .expect("probe runs")
                .is_empty()
        );
    }

    #[test]
    fn a_hardlink_to_this_program_is_still_this_program() {
        let _serial = crate::exclusive();
        let exe = std::env::current_exe().expect("exe");
        // Beside the test binary: /tmp is often another filesystem, where a
        // hardlink is refused.
        let dir = exe
            .parent()
            .expect("parent")
            .join(format!("self-hardlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let link = dir.join("ffmpeg");
        std::fs::hard_link(&exe, &link).expect("hardlink");
        assert!(resolves_to_this_program(&link));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_entry_that_cannot_run_is_skipped() {
        let _serial = crate::exclusive();
        let exe = std::env::current_exe().expect("exe");
        let base = exe
            .parent()
            .expect("parent")
            .join(format!("self-path-{}", std::process::id()));
        let dead = base.join("dead");
        std::fs::create_dir_all(&dead).expect("dir");
        std::fs::write(dead.join("ffmpeg"), "not executable").expect("write");
        let real = base.join("real");
        std::fs::create_dir_all(&real).expect("dir");
        std::fs::hard_link(&exe, real.join("ffmpeg")).expect("hardlink");
        let old = std::env::var_os("PATH");
        // SAFETY: guarded by the crate-wide test lock, so no other test reads
        // or writes the environment while this runs.
        unsafe {
            std::env::set_var("PATH", format!("{}:{}", dead.display(), real.display()));
        }
        let found = resolves_to_this_program(std::path::Path::new("ffmpeg"));
        // SAFETY: same lock, restored before the assertion.
        unsafe {
            match old {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
        let _ = std::fs::remove_dir_all(&base);
        assert!(found, "the guard stopped on a file the kernel would skip");
    }
}
