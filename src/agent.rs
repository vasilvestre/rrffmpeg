//! Agent transport: a small daemon on the worker replaces one ssh per job.
//!
//! A job is one TCP connection carrying length-prefixed frames: the client
//! sends stdin bytes, the daemon answers with stdout, stderr and the exit
//! code. A second kind of connection asks the daemon for status, which is how
//! the janitor tests a host and checks a job without a per-job probe.
//!
//! The daemon runs the program as its own user with the arguments it is given.
//! It is a remote code execution endpoint by design, so `serve` refuses to
//! start without a token. That token travels in clear text: bind the listener
//! to a private interface (WireGuard, Tailscale) and keep it off the LAN.
//! shortcut: no TLS, add rustls when the agent must cross an untrusted link.

use crate::config::Host;
use crate::transport::Transport;
use crate::{Error, Job, Program, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// Frame kinds. The first byte of every frame is one of these.
const HELLO: u8 = 0;
const STDIN: u8 = 1;
const STDOUT: u8 = 2;
const STDERR: u8 = 3;
const EXIT: u8 = 4;
const STDIN_EOF: u8 = 5;
const STATUS: u8 = 6;

/// A larger frame is a protocol error, not a big read.
const MAX_FRAME: usize = 1 << 20;

/// What the client says first on every connection.
#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    token: String,
    /// `exec` runs a job, `status` answers a probe and a liveness check.
    op: String,
    #[serde(default)]
    job: String,
    #[serde(default)]
    program: String,
    #[serde(default)]
    args: Vec<String>,
    /// Worker-side roots the daemon must be able to stat, for `probe`.
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Status {
    /// Every requested path is readable.
    ok: bool,
    /// The requested job is running under this daemon.
    alive: bool,
}

fn io_err(e: io::Error, what: &str) -> Error {
    Error::Io(e, what.to_string())
}

/// The lock cannot be held across a panic in this module, so a poisoned lock
/// still holds usable data. Never unwrap on a path a request can reach.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok((head[0], body))
}

fn write_frame(w: &mut impl Write, kind: u8, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&len.to_be_bytes());
    w.write_all(&head)?;
    w.write_all(body)?;
    w.flush()
}

/// Compare without leaking where the first difference is.
fn token_eq(given: &str, expected: &str) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.bytes().zip(expected.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// The client side of the protocol.
pub struct Agent {
    token: String,
    timeout: Duration,
}

impl Agent {
    pub fn new(token: String) -> Agent {
        Agent {
            token,
            timeout: Duration::from_secs(5),
        }
    }

    fn connect(&self, host: &Host) -> Result<TcpStream> {
        let name = host
            .agent_addr
            .as_deref()
            .ok_or_else(|| Error::Transport(format!("host {} has no agent_addr", host.name)))?;
        let addr: SocketAddr = name
            .to_socket_addrs()
            .map_err(|e| Error::Transport(format!("agent {name}: {e}")))?
            .next()
            .ok_or_else(|| Error::Transport(format!("agent {name} resolved to no address")))?;
        let stream = TcpStream::connect_timeout(&addr, self.timeout)
            .map_err(|e| Error::Transport(format!("agent {} at {name}: {e}", host.name)))?;
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    /// Send the hello and return the open connection, ready for frames.
    fn hello(&self, host: &Host, hello: &Hello) -> Result<TcpStream> {
        let mut stream = self.connect(host)?;
        // No read timeout after this point: a transcode can stay silent for
        // minutes while ffmpeg works, and the socket is the job's lifeline.
        stream.set_read_timeout(None).ok();
        let body =
            serde_json::to_vec(hello).map_err(|e| Error::Transport(format!("hello: {e}")))?;
        write_frame(&mut stream, HELLO, &body).map_err(|e| io_err(e, "sending hello"))?;
        Ok(stream)
    }

    /// Run one job with output going to the two sinks. `run` passes this
    /// process's own fds; the tests pass buffers.
    fn exec(
        &self,
        host: &Host,
        job: &Job,
        stderr_as_stdout: bool,
        stdin: Option<Box<dyn Read + Send>>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<i32> {
        let program = match job.program {
            Program::Ffmpeg => host.ffmpeg_bin.as_deref().unwrap_or("ffmpeg"),
            Program::Ffprobe => host.ffprobe_bin.as_deref().unwrap_or("ffprobe"),
        };
        let mut args = Vec::with_capacity(job.args.len());
        for arg in &job.args {
            // The protocol is JSON, so an argument that is not valid UTF-8
            // cannot be carried without corrupting it.
            let arg = arg.to_str().ok_or_else(|| {
                Error::Transport(format!("job {} has a non-UTF-8 argument", job.id))
            })?;
            args.push(arg.to_string());
        }

        let mut reader = self.hello(
            host,
            &Hello {
                token: self.token.clone(),
                op: "exec".into(),
                job: job.id.clone(),
                program: program.to_string(),
                args,
                paths: Vec::new(),
            },
        )?;

        let mut writer = reader
            .try_clone()
            .map_err(|e| io_err(e, "cloning the socket"))?;
        // The pump owns a clone, so dropping it never half-closes the job.
        std::thread::spawn(move || {
            if let Some(mut src) = stdin {
                let mut buf = vec![0u8; 32 * 1024];
                loop {
                    match src.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if write_frame(&mut writer, STDIN, &buf[..n]).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            let _ = write_frame(&mut writer, STDIN_EOF, &[]);
        });

        loop {
            match read_frame(&mut reader) {
                Ok((STDOUT, body)) => {
                    out.write_all(&body)
                        .map_err(|e| io_err(e, "writing stdout"))?;
                    let _ = out.flush();
                }
                Ok((STDERR, body)) => {
                    let sink: &mut dyn Write = if stderr_as_stdout {
                        &mut *out
                    } else {
                        &mut *err
                    };
                    sink.write_all(&body)
                        .map_err(|e| io_err(e, "writing stderr"))?;
                    let _ = sink.flush();
                }
                Ok((EXIT, body)) if body.len() == 4 => {
                    let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                    return Ok(code);
                }
                // Unknown frames are ignored so a newer daemon can add one.
                Ok(_) => {}
                // Same rule as ssh exit 255: a dropped connection is a
                // transport failure, never a completed job.
                Err(e) => {
                    return Err(Error::Transport(format!(
                        "agent {} dropped job {}: {e}",
                        host.name, job.id
                    )));
                }
            }
        }
    }

    fn status(&self, host: &Host, job: Option<&str>, paths: &[String]) -> Result<Status> {
        let mut stream = self.hello(
            host,
            &Hello {
                token: self.token.clone(),
                op: "status".into(),
                job: job.unwrap_or_default().to_string(),
                program: String::new(),
                args: Vec::new(),
                paths: paths.to_vec(),
            },
        )?;
        let timeout = self.timeout;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| io_err(e, "setting the read timeout"))?;
        let (kind, body) = read_frame(&mut stream)
            .map_err(|e| Error::Transport(format!("agent {}: {e}", host.name)))?;
        if kind != STATUS {
            return Err(Error::Transport(format!(
                "agent {} answered frame {kind}, expected status",
                host.name
            )));
        }
        serde_json::from_slice(&body)
            .map_err(|e| Error::Transport(format!("agent {} status: {e}", host.name)))
    }
}

impl Transport for Agent {
    fn run(&self, host: &Host, job: &Job, stderr_as_stdout: bool) -> Result<i32> {
        let stdin: Box<dyn Read + Send> = Box::new(io::stdin());
        let mut out = io::stdout().lock();
        let mut err = io::stderr().lock();
        self.exec(host, job, stderr_as_stdout, Some(stdin), &mut out, &mut err)
    }

    fn probe(&self, host: &Host) -> bool {
        // Same contract as ssh: every mapped root must be readable, so a dead
        // mount marks the host down instead of failing each job with ENOENT.
        let paths: Vec<String> = host
            .paths
            .iter()
            .map(|p| p.to.to_string_lossy().into_owned())
            .collect();
        matches!(self.status(host, None, &paths), Ok(s) if s.ok)
    }

    fn job_alive(&self, host: &Host, job_id: &str) -> bool {
        matches!(self.status(host, Some(job_id), &[]), Ok(s) if s.alive)
    }
}

/// Bind and serve until the process is killed. Refuses an empty token: the
/// daemon runs arbitrary argv, so an open port is an open shell.
pub fn serve(addr: &str, token: String) -> Result<()> {
    if token.is_empty() {
        return Err(Error::Config(
            "the agent needs a token, pass --token or RRFFMPEG_AGENT_TOKEN".into(),
        ));
    }
    let listener = TcpListener::bind(addr).map_err(|e| io_err(e, "binding the agent listener"))?;
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.to_string());
    eprintln!("rrffmpeg-agent: listening on {local}");
    serve_on(listener, token)
}

/// Same as `serve` with a caller-bound listener, so tests can use port 0.
pub fn serve_on(listener: TcpListener, token: String) -> Result<()> {
    let live: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("rrffmpeg-agent: accept: {e}");
                continue;
            }
        };
        let token = token.clone();
        let live = live.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &token, &live) {
                eprintln!("rrffmpeg-agent: {e}");
            }
        });
    }
    Ok(())
}

fn handle(stream: TcpStream, token: &str, live: &Arc<Mutex<HashMap<String, u32>>>) -> Result<()> {
    let mut reader = stream
        .try_clone()
        .map_err(|e| io_err(e, "cloning the socket"))?;
    // A client that connects and says nothing must not park a thread forever.
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| io_err(e, "setting the handshake timeout"))?;
    let (kind, body) =
        read_frame(&mut reader).map_err(|e| Error::Transport(format!("handshake: {e}")))?;
    if kind != HELLO {
        return Err(Error::Transport(format!(
            "first frame is {kind}, expected hello"
        )));
    }
    let hello: Hello =
        serde_json::from_slice(&body).map_err(|e| Error::Transport(format!("hello: {e}")))?;
    if !token_eq(&hello.token, token) {
        return Err(Error::Transport(format!(
            "bad token from {}",
            reader
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "unknown".into())
        )));
    }
    reader
        .set_read_timeout(None)
        .map_err(|e| io_err(e, "clearing the handshake timeout"))?;

    match hello.op.as_str() {
        "status" => status_reply(stream, &hello, live),
        "exec" => exec_reply(stream, &hello, live),
        other => Err(Error::Transport(format!("unknown op {other}"))),
    }
}

fn status_reply(
    mut stream: TcpStream,
    hello: &Hello,
    live: &Arc<Mutex<HashMap<String, u32>>>,
) -> Result<()> {
    let ok = hello.paths.iter().all(|p| std::fs::metadata(p).is_ok());
    let alive = if hello.job.is_empty() {
        false
    } else {
        lock(live)
            .get(&hello.job)
            .is_some_and(|pid| pid_alive(*pid))
    };
    let body = serde_json::to_vec(&Status { ok, alive })
        .map_err(|e| Error::Transport(format!("status: {e}")))?;
    write_frame(&mut stream, STATUS, &body).map_err(|e| io_err(e, "sending status"))
}

fn pid_alive(pid: u32) -> bool {
    // Signal 0 answers "does this pid exist" without touching the process.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn exec_reply(
    stream: TcpStream,
    hello: &Hello,
    live: &Arc<Mutex<HashMap<String, u32>>>,
) -> Result<()> {
    let mut child = Command::new(&hello.program)
        .args(&hello.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Transport(format!("agent cannot run {}: {e}", hello.program)))?;
    let pid = child.id();
    let stdin_pipe = child.stdin.take();
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| Error::Transport("agent child has no stdout".into()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| Error::Transport("agent child has no stderr".into()))?;

    let writer = Arc::new(Mutex::new(
        stream
            .try_clone()
            .map_err(|e| io_err(e, "cloning the socket"))?,
    ));
    lock(live).insert(hello.job.clone(), pid);

    // The waiter owns the child, drains both pipes, then reports the code and
    // shuts the socket down, which unblocks the stdin side below. The exit
    // frame must come after the pipes are empty or the last stderr line races
    // the code and the client drops it.
    let job = hello.job.clone();
    let waiter_live = live.clone();
    let waiter = writer.clone();
    std::thread::spawn(move || {
        let stdout_pump = pump(stdout_pipe, writer.clone(), STDOUT);
        let stderr_pump = pump(stderr_pipe, writer.clone(), STDERR);
        let code = child
            .wait()
            .map(|s| s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)))
            .unwrap_or(-1);
        let _ = stdout_pump.join();
        let _ = stderr_pump.join();
        let mut sock = lock(&waiter);
        let _ = write_frame(&mut *sock, EXIT, &code.to_be_bytes());
        let _ = sock.shutdown(Shutdown::Both);
        drop(sock);
        lock(&waiter_live).remove(&job);
    });

    let mut reader = stream;
    let mut stdin_pipe = stdin_pipe;
    loop {
        match read_frame(&mut reader) {
            Ok((STDIN, body)) => {
                if let Some(pipe) = stdin_pipe.as_mut() {
                    let _ = pipe.write_all(&body);
                    let _ = pipe.flush();
                }
            }
            // Jellyfin keeps stdin open and writes `q` at stop, so an early
            // EOF is not a reason to kill the job.
            Ok((STDIN_EOF, _)) => stdin_pipe = None,
            Ok(_) => {}
            // The client is gone: its stdio fds are closed with it.
            Err(_) => break,
        }
    }
    drop(stdin_pipe);

    // Kill the child only if the waiter has not already removed it. The
    // remaining race is a pid reused inside a microsecond window.
    if lock(live).remove(&hello.job).is_some() {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        std::thread::sleep(Duration::from_secs(2));
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    Ok(())
}

/// Copy one child pipe into frames of one kind.
fn pump(
    mut pipe: impl Read + Send + 'static,
    writer: Arc<Mutex<TcpStream>>,
    kind: u8,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 32 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if write_frame(&mut *lock(&writer), kind, &buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    const TOKEN: &str = "test-token";

    fn start() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            let _ = serve_on(listener, TOKEN.to_string());
        });
        addr
    }

    fn host(addr: &str, bin: &str) -> Host {
        Host {
            name: "worker".into(),
            concurrency: 1,
            capabilities: vec![],
            ffmpeg_bin: Some(bin.into()),
            ffprobe_bin: Some(bin.into()),
            agent_addr: Some(addr.into()),
            paths: vec![],
        }
    }

    fn job(args: &[&str]) -> Job {
        Job {
            id: format!("job-{}", std::process::id()),
            program: Program::Ffmpeg,
            args: args.iter().map(OsString::from).collect(),
            priority: 0,
            capability: None,
        }
    }

    fn agent() -> Agent {
        Agent::new(TOKEN.into())
    }

    fn exec(a: &Agent, h: &Host, j: &Job, input: Option<&str>) -> Result<(i32, String, String)> {
        let stdin: Option<Box<dyn Read + Send>> =
            input.map(|s| Box::new(io::Cursor::new(s.as_bytes().to_vec())) as Box<dyn Read + Send>);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = a.exec(h, j, false, stdin, &mut out, &mut err)?;
        Ok((
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ))
    }

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDIN, b"hello").expect("write");
        write_frame(&mut buf, STDIN_EOF, b"").expect("write");
        let mut r = buf.as_slice();
        assert_eq!(
            read_frame(&mut r).expect("read"),
            (STDIN, b"hello".to_vec())
        );
        assert_eq!(read_frame(&mut r).expect("read"), (STDIN_EOF, Vec::new()));
    }

    #[test]
    fn a_frame_longer_than_the_cap_is_rejected() {
        let mut buf = vec![STDOUT];
        buf.extend_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        let err = read_frame(&mut buf.as_slice()).expect_err("oversized frame");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_job_returns_stdout_and_exit_code() {
        let addr = start();
        let h = host(&addr, "/bin/echo");
        let (code, out, err) = exec(&agent(), &h, &job(&["hello"]), None).expect("exec");
        assert_eq!(code, 0);
        assert_eq!(out, "hello\n");
        assert_eq!(err, "");
    }

    #[test]
    fn stdin_reaches_the_child() {
        let addr = start();
        let h = host(&addr, "/bin/cat");
        let (code, out, _) = exec(&agent(), &h, &job(&[]), Some("ping")).expect("exec");
        assert_eq!(code, 0);
        assert_eq!(out, "ping");
    }

    #[test]
    fn stderr_and_the_exit_code_come_back_separately() {
        let addr = start();
        let h = host(&addr, "/bin/sh");
        let (code, out, err) =
            exec(&agent(), &h, &job(&["-c", "printf err >&2; exit 3"]), None).expect("exec");
        assert_eq!(code, 3);
        assert_eq!(out, "");
        assert_eq!(err, "err");
    }

    #[test]
    fn stderr_as_stdout_merges_on_the_client_side() {
        let addr = start();
        let h = host(&addr, "/bin/sh");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = agent()
            .exec(
                &h,
                &job(&["-c", "printf both >&2"]),
                true,
                None,
                &mut out,
                &mut err,
            )
            .expect("exec");
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8_lossy(&out), "both");
        assert!(err.is_empty());
    }

    #[test]
    fn probe_checks_the_mapped_roots() {
        let addr = start();
        let mut h = host(&addr, "/bin/true");
        assert!(agent().probe(&h));

        let dir = std::env::temp_dir().join("rrffmpeg-agent-probe");
        std::fs::create_dir_all(&dir).expect("temp dir");
        h.paths = vec![crate::PathMap {
            from: PathBuf::from("/client"),
            to: dir,
        }];
        assert!(agent().probe(&h));

        h.paths[0].to = PathBuf::from("/nonexistent/rrffmpeg-mount");
        assert!(!agent().probe(&h));
    }

    #[test]
    fn an_unknown_job_is_not_alive() {
        let addr = start();
        let h = host(&addr, "/bin/true");
        assert!(!agent().job_alive(&h, "nobody"));
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let addr = start();
        let h = host(&addr, "/bin/echo");
        let bad = Agent::new("wrong".into());
        assert!(exec(&bad, &h, &job(&["x"]), None).is_err());
        assert!(!bad.probe(&h));
    }

    #[test]
    fn an_unreachable_agent_is_a_transport_error() {
        let h = host("127.0.0.1:1", "/bin/echo");
        assert!(exec(&agent(), &h, &job(&["x"]), None).is_err());
        assert!(!agent().probe(&h));
    }
}
