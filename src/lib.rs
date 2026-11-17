//! Dispatch core shared by the binary and the tests.

pub mod config;
pub mod janitor;
pub mod paths;
pub mod scheduler;
pub mod state;
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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e, what) => write!(f, "rrffmpeg: {what}: {e}"),
            Error::Config(m) => write!(f, "rrffmpeg: config: {m}"),
            Error::State(m) => write!(f, "rrffmpeg: state: {m}"),
            Error::Transport(m) => write!(f, "rrffmpeg: transport: {m}"),
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
