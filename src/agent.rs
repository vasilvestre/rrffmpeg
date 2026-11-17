//! Agent transport: a small daemon on the worker replaces one ssh per job.
//!
//! A job uses two TLS connections:
//!
//! - the control connection carries the hello, a `ready` frame, then stdout,
//!   stderr and the exit code from the daemon;
//! - the stdin connection carries the bytes Jellyfin writes to stdin.
//!
//! One connection per direction is what keeps each stream owned by a single
//! thread. TLS needs one object for both directions, so a shared stream means
//! a lock, and a lock held by a blocked reader starves the writer. Two
//! connections cost one handshake and remove the problem.
//!
//! The daemon runs the program as its own user with the arguments it is given.
//! It is a remote code execution endpoint by design, so `serve` refuses to
//! start without a token and without a certificate. The client trusts exactly
//! one certificate, the daemon's own: no CA, no web PKI.

use crate::config::Host;
use crate::transport::Transport;
use crate::{Error, Job, Program, Result};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection, StreamOwned};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Frame kinds. The first byte of every frame is one of these.
const HELLO: u8 = 0;
const STDIN: u8 = 1;
const STDOUT: u8 = 2;
const STDERR: u8 = 3;
const EXIT: u8 = 4;
const STDIN_EOF: u8 = 5;
const STATUS: u8 = 6;
const READY: u8 = 7;

/// A larger frame is a protocol error, not a big read.
const MAX_FRAME: usize = 1 << 20;

/// Connect and handshake budget. A silent peer must not park a thread.
const HANDSHAKE: Duration = Duration::from_secs(5);

/// Frames buffered between a child pipe and the socket. A full queue blocks
/// the pump, the pipe fills, and ffmpeg waits: that is the backpressure.
const QUEUE: usize = 64;

/// Revision of the client/daemon protocol. Bump it when a frame or a field
/// changes meaning; the daemon refuses a peer that speaks another revision.
const PROTOCOL: u32 = 1;

/// What the client says first on every connection.
#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    /// Left at 0 by a client older than this field, so the daemon can say so.
    #[serde(default)]
    version: u32,
    token: String,
    /// `exec` runs a job on the control connection, `stdin` carries its input,
    /// `status` answers a probe and a liveness check.
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

/// One running job on the daemon: its pid, and its stdin until the stdin
/// connection claims it.
struct Slot {
    pid: u32,
    stdin: Option<ChildStdin>,
}

type Live = Arc<Mutex<HashMap<String, Slot>>>;

fn io_err(e: io::Error, what: &str) -> Error {
    Error::Io(e, what.to_string())
}

/// Reclassify a transport failure that happened before the daemon started the
/// process, so the caller may retry it elsewhere.
fn not_started(e: Error) -> Error {
    match e {
        Error::Transport(m) => Error::NotStarted(m),
        other => other,
    }
}

/// The lock cannot be held across a panic in this module, so a poisoned lock
/// still holds usable data. Never unwrap on a path a request can reach.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

/// Bytes to frames. A read can stop in the middle of a header or a body, so
/// the pieces are kept here instead of in a `read_exact` stack frame.
#[derive(Default)]
struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn next(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let kind = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let body = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some((kind, body)))
    }
}

/// Read one frame, waiting no longer than `deadline` if one is given.
fn recv(
    stream: &mut Stream,
    framer: &mut Framer,
    deadline: Option<Instant>,
) -> io::Result<(u8, Vec<u8>)> {
    loop {
        if let Some(frame) = framer.next()? {
            return Ok(frame);
        }
        if let Some(end) = deadline {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "no answer from the peer",
                ));
            }
            stream.set_read_timeout(Some(left))?;
        }
        let mut chunk = [0u8; 32 * 1024];
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed the connection",
                ));
            }
            Ok(n) => framer.push(&chunk[..n]),
            Err(e) => return Err(e),
        }
    }
}

/// A TLS stream, client or server side. One type so the framing code above
/// does not care which end it is on.
enum Stream {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

impl Stream {
    fn sock(&self) -> &TcpStream {
        match self {
            Stream::Client(s) => &s.sock,
            Stream::Server(s) => &s.sock,
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.sock().set_read_timeout(timeout)
    }

    /// Say goodbye instead of walking away. A bare close makes the peer see a
    /// reset if it was still writing session tickets, and that lands in its
    /// log as an error. Never reads, so it cannot hang on a mute peer.
    fn close(mut self) {
        match &mut self {
            Stream::Client(s) => {
                s.conn.send_close_notify();
                while s.conn.wants_write() {
                    if s.conn.write_tls(&mut s.sock).is_err() {
                        return;
                    }
                }
            }
            Stream::Server(s) => {
                s.conn.send_close_notify();
                while s.conn.wants_write() {
                    if s.conn.write_tls(&mut s.sock).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Client(s) => s.read(buf),
            Stream::Server(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Client(s) => s.write(buf),
            Stream::Server(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Client(s) => s.flush(),
            Stream::Server(s) => s.flush(),
        }
    }
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

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::Io(e, format!("reading {}", path.display())))
}

fn pem_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let text = read_file(path)?;
    let certs = rustls_pemfile::certs(&mut io::Cursor::new(text))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    if certs.is_empty() {
        return Err(Error::Config(format!(
            "{} holds no certificate",
            path.display()
        )));
    }
    Ok(certs)
}

/// Trust exactly the certificate in the file. A self-signed certificate names
/// the addresses in its subject alternative names, so the usual checks still
/// run: expiry, and the name the client asked for.
fn client_config(cert: &Path) -> Result<Arc<ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    for der in pem_certs(cert)? {
        roots
            .add(der)
            .map_err(|e| Error::Config(format!("{}: {e}", cert.display())))?;
    }
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Config(format!("tls: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Build the daemon side from the two PEM files. Public so `serve` and the
/// latency test agree on one code path.
pub fn server_config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>> {
    let certs = pem_certs(cert)?;
    let text = read_file(key)?;
    let key = rustls_pemfile::private_key(&mut io::Cursor::new(text))
        .map_err(|e| Error::Config(format!("{}: {e}", key.display())))?
        .ok_or_else(|| Error::Config(format!("{} holds no private key", key.display())))?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Config(format!("tls: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::Config(format!("tls: {e}")))?;
    Ok(Arc::new(config))
}

/// The name to verify in the certificate: the host part of `agent_addr`, so
/// the certificate must list that address or name in its SANs.
fn server_name(addr: &str) -> Result<ServerName<'static>> {
    let host = match addr.rsplit_once(':') {
        Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
        None => addr,
    };
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(host.to_owned())
        .map_err(|e| Error::Config(format!("agent address {host}: {e}")))
}

/// Write a certificate and its key into `dir`, for the worker and for tests.
/// The names are `cert.pem` and `key.pem`.
pub fn generate_cert(dir: &Path, sans: &[String]) -> Result<()> {
    if sans.is_empty() {
        return Err(Error::Config(
            "a certificate needs at least one name, pass --san".into(),
        ));
    }
    let key = rcgen::KeyPair::generate().map_err(|e| Error::Config(format!("cert: {e}")))?;
    let mut params = rcgen::CertificateParams::new(sans.to_vec())
        .map_err(|e| Error::Config(format!("cert: {e}")))?;
    // A lab box outlives a 90 day certificate, and nothing here rotates it.
    params.not_after = rcgen::date_time_ymd(2126, 1, 1);
    let cert = params
        .self_signed(&key)
        .map_err(|e| Error::Config(format!("cert: {e}")))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Io(e, format!("creating {}", dir.display())))?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem())
        .map_err(|e| Error::Io(e, format!("writing {}", cert_path.display())))?;
    std::fs::write(&key_path, key.serialize_pem())
        .map_err(|e| Error::Io(e, format!("writing {}", key_path.display())))?;
    // The key is a secret: one file, one user.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Io(e, format!("chmod {}", key_path.display())))?;
    Ok(())
}

/// The client side of the protocol.
pub struct Agent {
    token: String,
    timeout: Duration,
    config: Arc<ClientConfig>,
}

impl Agent {
    /// `cert` is the PEM of the daemon certificate, the only trust anchor.
    pub fn load(token: String, cert: &Path) -> Result<Agent> {
        Ok(Agent {
            token,
            timeout: HANDSHAKE,
            config: client_config(cert)?,
        })
    }

    /// Connect and shake hands. The socket has no read timeout after this,
    /// except where the caller sets one: a transcode can stay silent for
    /// minutes while ffmpeg works.
    fn open(&self, host: &Host, timeout: Option<Duration>) -> Result<Stream> {
        let name = host
            .agent_addr
            .as_deref()
            .ok_or_else(|| Error::Transport(format!("host {} has no agent_addr", host.name)))?;
        let addr: SocketAddr = name
            .to_socket_addrs()
            .map_err(|e| Error::Transport(format!("agent {name}: {e}")))?
            .next()
            .ok_or_else(|| Error::Transport(format!("agent {name} resolved to no address")))?;
        let sock = TcpStream::connect_timeout(&addr, self.timeout)
            .map_err(|e| Error::Transport(format!("agent {} at {name}: {e}", host.name)))?;
        let _ = sock.set_nodelay(true);
        sock.set_read_timeout(Some(self.timeout))
            .map_err(|e| io_err(e, "setting the handshake timeout"))?;
        let conn = ClientConnection::new(self.config.clone(), server_name(name)?)
            .map_err(|e| Error::Transport(format!("agent {}: {e}", host.name)))?;
        let mut stream = Stream::Client(StreamOwned::new(conn, sock));
        while let Stream::Client(s) = &mut stream {
            if !s.conn.is_handshaking() {
                break;
            }
            s.conn
                .complete_io(&mut s.sock)
                .map_err(|e| Error::Transport(format!("agent {} handshake: {e}", host.name)))?;
        }
        stream
            .set_read_timeout(timeout)
            .map_err(|e| io_err(e, "setting the read timeout"))?;
        Ok(stream)
    }

    /// Send the hello and return the open connection, ready for frames.
    fn hello(&self, host: &Host, hello: &Hello, timeout: Option<Duration>) -> Result<Stream> {
        let mut stream = self.open(host, timeout)?;
        let body =
            serde_json::to_vec(hello).map_err(|e| Error::Transport(format!("hello: {e}")))?;
        write_frame(&mut stream, HELLO, &body)
            .map_err(|e| Error::Transport(format!("agent {}: {e}", host.name)))?;
        Ok(stream)
    }

    /// The stdin connection: one thread writes it, nobody reads it here.
    fn stdin_thread(&self, host: Host, job_id: String, stdin: Option<Box<dyn Read + Send>>) {
        let hello = Hello {
            version: PROTOCOL,
            token: self.token.clone(),
            op: "stdin".into(),
            job: job_id.clone(),
            program: String::new(),
            args: Vec::new(),
            paths: Vec::new(),
        };
        let mut stream = match self.hello(&host, &hello, None) {
            Ok(s) => s,
            Err(e) => {
                // shortcut: without this connection the job runs with no
                // stdin, so Jellyfin's `q` is lost. It costs a transcode, not
                // an answer, so log it and let the job finish.
                crate::log_at!(crate::log::Level::Warn, "agent {} stdin: {e}", host.name);
                return;
            }
        };
        if let Some(mut src) = stdin {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match src.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if write_frame(&mut stream, STDIN, &buf[..n]).is_err() {
                            return;
                        }
                    }
                }
            }
        }
        if write_frame(&mut stream, STDIN_EOF, &[]).is_err() {
            return;
        }
        // Hold the connection open until the daemon closes it. Closing early
        // would read as "the client died" and get the job killed.
        let mut buf = [0u8; 256];
        while matches!(stream.read(&mut buf), Ok(n) if n > 0) {}
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
        let started = Instant::now();
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

        let hello = Hello {
            version: PROTOCOL,
            token: self.token.clone(),
            op: "exec".into(),
            job: job.id.clone(),
            program: program.to_string(),
            args,
            paths: Vec::new(),
        };
        // `ready` is the line between "the daemon ran it" and "it never
        // started". Before it, nothing was written anywhere, so a retry on
        // another host cannot duplicate output.
        let mut ctl = self
            .hello(host, &hello, Some(self.timeout))
            .map_err(not_started)?;
        let mut framer = Framer::default();
        match recv(&mut ctl, &mut framer, Some(Instant::now() + self.timeout)) {
            Ok((READY, _)) => {}
            Ok((EXIT, body)) if body.len() == 4 => {
                // The daemon refused the job but still reported a code.
                return Ok(i32::from_be_bytes([body[0], body[1], body[2], body[3]]));
            }
            Ok((kind, _)) => {
                return Err(Error::Transport(format!(
                    "agent {} answered frame {kind}, expected ready",
                    host.name
                )));
            }
            Err(e) => {
                return Err(Error::NotStarted(format!(
                    "agent {} did not start job {}: {e}",
                    host.name, job.id
                )));
            }
        }
        ctl.set_read_timeout(None)
            .map_err(|e| io_err(e, "clearing the read timeout"))?;

        // The daemon has the job now, so the child is waiting on stdin.
        let stdin_host = host.clone();
        let id = job.id.clone();
        let me = Agent {
            token: self.token.clone(),
            timeout: self.timeout,
            config: self.config.clone(),
        };
        std::thread::spawn(move || me.stdin_thread(stdin_host, id, stdin));

        let mut stdout_bytes = 0usize;
        let mut stderr_bytes = 0usize;
        loop {
            match recv(&mut ctl, &mut framer, None) {
                Ok((STDOUT, body)) => {
                    stdout_bytes += body.len();
                    out.write_all(&body)
                        .map_err(|e| io_err(e, "writing stdout"))?;
                    let _ = out.flush();
                }
                Ok((STDERR, body)) => {
                    stderr_bytes += body.len();
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
                    crate::log_at!(
                        crate::log::Level::Debug,
                        "job {} relayed {} B stdout, {} B stderr in {} ms",
                        job.id,
                        stdout_bytes,
                        stderr_bytes,
                        started.elapsed().as_millis()
                    );
                    ctl.close();
                    return Ok(i32::from_be_bytes([body[0], body[1], body[2], body[3]]));
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
                version: PROTOCOL,
                token: self.token.clone(),
                op: "status".into(),
                job: job.unwrap_or_default().to_string(),
                program: String::new(),
                args: Vec::new(),
                paths: paths.to_vec(),
            },
            Some(self.timeout),
        )?;
        let mut framer = Framer::default();
        let (kind, body) = recv(
            &mut stream,
            &mut framer,
            Some(Instant::now() + self.timeout),
        )
        .map_err(|e| Error::Transport(format!("agent {}: {e}", host.name)))?;
        if kind != STATUS {
            return Err(Error::Transport(format!(
                "agent {} answered frame {kind}, expected status",
                host.name
            )));
        }
        let status: Status = serde_json::from_slice(&body)
            .map_err(|e| Error::Transport(format!("agent {} status: {e}", host.name)))?;
        stream.close();
        Ok(status)
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

/// Bind and serve until the process is killed. Refuses an empty token or a
/// missing certificate: the daemon runs arbitrary argv, so an open port
/// without TLS is an open shell.
pub fn serve(addr: &str, token: String, cert: &Path, key: &Path) -> Result<()> {
    if token.is_empty() {
        return Err(Error::Config(
            "the agent needs a token, pass --token or RRFFMPEG_AGENT_TOKEN".into(),
        ));
    }
    let tls = server_config(cert, key)?;
    let listener = TcpListener::bind(addr).map_err(|e| io_err(e, "binding the agent listener"))?;
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.to_string());
    eprintln!("rrffmpeg-agent: listening on {local} with tls");
    serve_on(listener, token, tls)
}

/// Same as `serve` with a caller-bound listener and a prepared config, so
/// tests can use port 0 and a throwaway certificate.
pub fn serve_on(listener: TcpListener, token: String, tls: Arc<ServerConfig>) -> Result<()> {
    let live: Live = Arc::new(Mutex::new(HashMap::new()));
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
        let tls = tls.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, &token, &live, tls) {
                // A client that leaves without close_notify is normal: it
                // exits when the job ends.
                if !ends_quietly(&e) {
                    eprintln!("rrffmpeg-agent: {e}");
                }
            }
        });
    }
    Ok(())
}

/// True for the errors a client that simply stops talking produces.
fn ends_quietly(e: &Error) -> bool {
    match e {
        Error::Transport(m) => {
            m.contains("peer closed the connection")
                || m.contains("close_notify")
                || m.contains("unexpected end of file")
        }
        _ => false,
    }
}

fn handle(stream: TcpStream, token: &str, live: &Live, tls: Arc<ServerConfig>) -> Result<()> {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut stream = accept(stream, tls, &peer)?;
    let mut framer = Framer::default();
    let (kind, body) = recv(&mut stream, &mut framer, Some(Instant::now() + HANDSHAKE))
        .map_err(|e| Error::Transport(format!("handshake from {peer}: {e}")))?;
    if kind != HELLO {
        return Err(Error::Transport(format!(
            "first frame is {kind}, expected hello"
        )));
    }
    let hello: Hello =
        serde_json::from_slice(&body).map_err(|e| Error::Transport(format!("hello: {e}")))?;
    check_protocol(hello.version, &peer)?;
    if !token_eq(&hello.token, token) {
        return Err(Error::Transport(format!("bad token from {peer}")));
    }
    stream
        .set_read_timeout(None)
        .map_err(|e| io_err(e, "clearing the handshake timeout"))?;

    match hello.op.as_str() {
        "status" => status_reply(&mut stream, &hello, live),
        "exec" => exec_reply(stream, &hello, live),
        "stdin" => stdin_reply(stream, framer, &hello, live),
        other => Err(Error::Transport(format!("unknown op {other}"))),
    }
}

/// Refuse a peer built from another revision before any frame is interpreted.
fn check_protocol(version: u32, peer: &str) -> Result<()> {
    if version == PROTOCOL {
        return Ok(());
    }
    Err(Error::Transport(format!(
        "agent protocol {version} from {peer}, this daemon speaks {PROTOCOL}: install the same release on the client and the worker"
    )))
}

/// Complete the handshake, then let the caller decide about timeouts.
fn accept(sock: TcpStream, tls: Arc<ServerConfig>, peer: &str) -> Result<Stream> {
    sock.set_read_timeout(Some(HANDSHAKE))
        .map_err(|e| io_err(e, "setting the handshake timeout"))?;
    let conn = ServerConnection::new(tls)
        .map_err(|e| Error::Transport(format!("tls from {peer}: {e}")))?;
    let mut stream = Stream::Server(StreamOwned::new(conn, sock));
    while let Stream::Server(s) = &mut stream {
        if !s.conn.is_handshaking() {
            break;
        }
        s.conn
            .complete_io(&mut s.sock)
            .map_err(|e| Error::Transport(format!("tls from {peer}: {e}")))?;
    }
    Ok(stream)
}

fn status_reply(stream: &mut Stream, hello: &Hello, live: &Live) -> Result<()> {
    let ok = hello.paths.iter().all(|p| std::fs::metadata(p).is_ok());
    let alive = if hello.job.is_empty() {
        false
    } else {
        lock(live)
            .get(&hello.job)
            .is_some_and(|slot| pid_alive(slot.pid))
    };
    let body = serde_json::to_vec(&Status { ok, alive })
        .map_err(|e| Error::Transport(format!("status: {e}")))?;
    write_frame(stream, STATUS, &body).map_err(|e| io_err(e, "sending status"))
}

fn pid_alive(pid: u32) -> bool {
    // Signal 0 answers "does this pid exist" without touching the process.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

fn exec_reply(mut stream: Stream, hello: &Hello, live: &Live) -> Result<()> {
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

    lock(live).insert(
        hello.job.clone(),
        Slot {
            pid,
            stdin: stdin_pipe,
        },
    );
    // The client waits for this before it opens the stdin connection, so the
    // job is in the registry before anyone looks for it.
    write_frame(&mut stream, READY, &[]).map_err(|e| io_err(e, "sending ready"))?;

    // The waiter owns the child and both pipes. Frames go through this queue
    // so exactly one thread, the one below, writes the socket.
    let (tx, rx) = sync_channel::<(u8, Vec<u8>)>(QUEUE);
    let job = hello.job.clone();
    let waiter_live = live.clone();
    std::thread::spawn(move || {
        let stdout_pump = pump(stdout_pipe, tx.clone(), STDOUT);
        let stderr_pump = pump(stderr_pipe, tx.clone(), STDERR);
        let code = child
            .wait()
            .map(|s| s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)))
            .unwrap_or(-1);
        let _ = stdout_pump.join();
        let _ = stderr_pump.join();
        // The exit frame must come after the pipes are empty or the last
        // stderr line races the code and the client drops it.
        let _ = tx.send((EXIT, code.to_be_bytes().to_vec()));
        lock(&waiter_live).remove(&job);
    });

    while let Ok((kind, body)) = rx.recv() {
        if write_frame(&mut stream, kind, &body).is_err() {
            break;
        }
    }
    Ok(())
}

/// The stdin connection. Its end is the client's end: a client that goes away
/// closes it, and the job must not outlive the person waiting for it.
fn stdin_reply(mut stream: Stream, mut framer: Framer, hello: &Hello, live: &Live) -> Result<()> {
    let Some(pipe) = lock(live)
        .get_mut(&hello.job)
        .and_then(|slot| slot.stdin.take())
    else {
        // A job that ends before its stdin connection arrives is normal: the
        // client opens stdin right after `ready`, and a short job can finish
        // in between. Nothing to feed, and nothing to kill.
        return Ok(());
    };
    let mut pipe = Some(pipe);
    loop {
        match recv(&mut stream, &mut framer, None) {
            Ok((STDIN, body)) => {
                if let Some(pipe) = pipe.as_mut() {
                    let _ = pipe.write_all(&body);
                    let _ = pipe.flush();
                }
            }
            // Jellyfin keeps stdin open and writes `q` at stop, so an early
            // EOF is not a reason to kill the job.
            Ok((STDIN_EOF, _)) => pipe = None,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    drop(pipe);

    // Kill the child only if the waiter has not already removed it. The
    // remaining race is a pid reused inside a microsecond window.
    if let Some(slot) = lock(live).remove(&hello.job) {
        unsafe { libc::kill(slot.pid as libc::pid_t, libc::SIGTERM) };
        std::thread::sleep(Duration::from_secs(2));
        if pid_alive(slot.pid) {
            unsafe { libc::kill(slot.pid as libc::pid_t, libc::SIGKILL) };
        }
    }
    Ok(())
}

/// Copy one child pipe into frames of one kind.
fn pump(
    pipe: impl Read + Send + 'static,
    tx: SyncSender<(u8, Vec<u8>)>,
    kind: u8,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut buf = [0u8; 32 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send((kind, buf[..n].to_vec())).is_err() {
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

    /// One throwaway certificate for the whole test binary, generated in a
    /// temp dir. Nothing secret is committed.
    fn tls() -> (Arc<ServerConfig>, PathBuf) {
        static ONCE: std::sync::OnceLock<(Arc<ServerConfig>, PathBuf)> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("rrffmpeg-agent-tls-{}", std::process::id()));
            let sans = vec!["127.0.0.1".to_string(), "localhost".to_string()];
            generate_cert(&dir, &sans).expect("cert");
            let config =
                server_config(&dir.join("cert.pem"), &dir.join("key.pem")).expect("server config");
            (config, dir.join("cert.pem"))
        })
        .clone()
    }

    fn start() -> String {
        let (config, _) = tls();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        std::thread::spawn(move || {
            let _ = serve_on(listener, TOKEN.to_string(), config);
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
        let (_, cert) = tls();
        Agent::load(TOKEN.into(), &cert).expect("agent")
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
    fn frames_round_trip_across_partial_reads() {
        let mut buf = Vec::new();
        write_frame(&mut buf, STDIN, b"hello").expect("write");
        write_frame(&mut buf, STDIN_EOF, b"").expect("write");

        let mut framer = Framer::default();
        // One byte at a time: the worst case a timeout can produce.
        for (i, byte) in buf.iter().enumerate() {
            framer.push(&[*byte]);
            if i < 9 {
                assert!(framer.next().expect("no error").is_none());
            }
        }
        assert_eq!(
            framer.next().expect("read").expect("frame"),
            (STDIN, b"hello".to_vec())
        );
        assert_eq!(
            framer.next().expect("read").expect("frame"),
            (STDIN_EOF, Vec::new())
        );
    }

    #[test]
    fn a_frame_longer_than_the_cap_is_rejected() {
        let mut framer = Framer::default();
        let mut buf = vec![STDOUT];
        buf.extend_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        framer.push(&buf);
        let err = framer.next().expect_err("oversized frame");
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
    fn a_large_output_does_not_deadlock() {
        let addr = start();
        let h = host(&addr, "/bin/sh");
        let (code, out, _) = exec(
            &agent(),
            &h,
            &job(&["-c", "head -c 300000 /dev/zero | tr '\\0' x"]),
            None,
        )
        .expect("exec");
        assert_eq!(code, 0);
        assert_eq!(out.len(), 300_000);
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
        let (_, cert) = tls();
        let bad = Agent::load("wrong".into(), &cert).expect("agent");
        assert!(exec(&bad, &h, &job(&["x"]), None).is_err());
        assert!(!bad.probe(&h));
    }

    #[test]
    fn a_plain_tcp_client_cannot_talk_to_the_agent() {
        let addr = start();
        let mut sock = TcpStream::connect(&addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        // A cleartext hello is not a TLS record. The daemon must hang up
        // without ever answering a frame.
        write_frame(
            &mut sock,
            HELLO,
            b"{\"token\":\"test-token\",\"op\":\"status\"}",
        )
        .ok();
        let mut buf = [0u8; 64];
        let mut total = 0usize;
        loop {
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n;
                    assert!(total < 4096, "the daemon keeps talking plaintext");
                }
            }
        }
        assert!(total < 4096);
    }

    #[test]
    fn a_certificate_for_another_name_is_refused() {
        let addr = start();
        let dir = std::env::temp_dir().join(format!("rrffmpeg-wrong-name-{}", std::process::id()));
        generate_cert(&dir, &["other.example".to_string()]).expect("cert");
        // Trusting another certificate must not open the connection, even
        // though both are self-signed.
        let wrong = Agent::load(TOKEN.into(), &dir.join("cert.pem")).expect("agent");
        assert!(!wrong.probe(&host(&addr, "/bin/echo")));
        assert!(!wrong.job_alive(&host(&addr, "/bin/echo"), "x"));
    }

    #[test]
    fn an_unreachable_agent_is_a_transport_error() {
        let h = host("127.0.0.1:1", "/bin/echo");
        assert!(exec(&agent(), &h, &job(&["x"]), None).is_err());
        assert!(!agent().probe(&h));
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn a_peer_from_another_revision_is_refused_by_name() {
        assert!(check_protocol(PROTOCOL, "worker").is_ok());
        // A client older than the field sends no version at all.
        let err = check_protocol(0, "worker").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("protocol 0"), "{text}");
        assert!(text.contains("this daemon speaks 1"), "{text}");
    }

    #[test]
    fn an_old_client_hello_parses_as_revision_zero() {
        let hello: Hello =
            serde_json::from_str(r#"{"token":"t","op":"status"}"#).expect("old hello");
        assert_eq!(hello.version, 0);
    }
}
