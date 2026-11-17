//! Dispatch core shared by the binary and the tests.

pub mod agent;
pub mod config;
pub mod janitor;
pub mod locks;
pub mod log;
pub mod paths;
pub mod queue;
pub mod scheduler;
pub mod state;
pub mod stats;
pub mod transport;

pub use config::{Config, Host, PathMap, Program};

use std::ffi::OsString;
use std::fmt;

/// One dispatched transcode.
///
/// `id` is unique across restarts, so a row left behind by a killed client
/// can never be mistaken for a live job.
#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub program: Program,
    pub args: Vec<OsString>,
    /// Lower runs first: probes and seeks beat long transcodes.
    pub priority: i32,
    /// Capability the worker must advertise, for example `vaapi`.
    pub capability: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error, String),
    Config(String),
    State(String),
    Transport(String),
    /// The worker never started the job. The only error a retry on another
    /// host can take without risking duplicated output.
    NotStarted(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e, what) => write!(f, "rrffmpeg: {what}: {e}"),
            Error::Config(m) => write!(f, "rrffmpeg: config: {m}"),
            Error::State(m) => write!(f, "rrffmpeg: state: {m}"),
            Error::Transport(m) => write!(f, "rrffmpeg: transport: {m}"),
            Error::NotStarted(m) => write!(f, "rrffmpeg: not started: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e, "io".into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// One test at a time for the tests that write executables or spawn processes.
///
/// `fork` copies the whole descriptor table, so a child forked by another test
/// thread keeps a script this thread is writing open for writing until it execs.
/// Executing that script then fails with `ETXTBSY`. There is no per-test way to
/// avoid the copy, so those tests take this lock for their whole body.
#[cfg(test)]
pub(crate) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
