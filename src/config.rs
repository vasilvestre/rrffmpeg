//! Worker list and paths, loaded from a TOML file.
//!
//! The file lives at `$RRFFMPEG_CONFIG`, else `$XDG_CONFIG_HOME/rffmpeg/config.toml`,
//! else `~/.config/rffmpeg/config.toml`. A missing file means "no workers", and
//! the dispatcher then runs ffmpeg locally.

use crate::{Error, Result};
use serde::Deserialize;
use std::path::PathBuf;

fn one() -> usize {
    1
}

fn default_state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("rrffmpeg")
}

/// How long a job waits for a free worker slot before running locally.
fn default_queue_wait() -> u64 {
    600
}

/// How long one process's "host down" verdict is trusted by the others.
/// Expiry is the whole repair mechanism: no daemon clears the flag.
fn default_down_ttl() -> u64 {
    60
}

fn default_ssh() -> PathBuf {
    PathBuf::from("ssh")
}

/// Clients sharing a state_dir are told apart by this name, so the hostname is
/// the least surprising default. Two hosts named alike count as one client.
fn default_client_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "client".to_string())
}

/// Same scale as the kernel's BFQ IO weight, so one client's number means the
/// same thing here and in `blkio.bfq.weight`.
fn default_client_weight() -> u32 {
    100
}

fn default_ffmpeg() -> PathBuf {
    PathBuf::from("ffmpeg")
}

fn default_ffprobe() -> PathBuf {
    PathBuf::from("ffprobe")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Program {
    Ffmpeg,
    Ffprobe,
}

impl Program {
    /// Name of the tool on the worker, and the local fallback.
    pub fn binary(self) -> &'static str {
        match self {
            Program::Ffmpeg => "ffmpeg",
            Program::Ffprobe => "ffprobe",
        }
    }
}

/// Rewrites one path prefix. The longest matching `from` wins.
#[derive(Debug, Clone, Deserialize)]
pub struct PathMap {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Host {
    /// `ssh` destination: `worker`, `user@worker`, or an IP.
    pub name: String,
    /// Maximum jobs sent to this host at once.
    #[serde(default = "one")]
    pub concurrency: usize,
    /// Advertised accelerators: `vaapi`, `nvenc`, `qsv`. Empty accepts any job.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Remote ffmpeg command. Defaults to `ffmpeg` from the worker PATH; point
    /// it at a wrapper that sets the OpenCL environment.
    #[serde(default)]
    pub ffmpeg_bin: Option<String>,
    #[serde(default)]
    pub ffprobe_bin: Option<String>,
    /// `host:port` of the agent daemon. Set it to skip ssh for this host.
    #[serde(default)]
    pub agent_addr: Option<String>,
    #[serde(default)]
    pub paths: Vec<PathMap>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<Host>,
    /// Send ffmpeg's stderr to stdout, as Jellyfin expects for some builds.
    #[serde(default)]
    pub stderr_as_stdout: bool,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_ssh")]
    pub ssh_bin: PathBuf,
    /// Shared secret of the agent daemon. `RRFFMPEG_AGENT_TOKEN` wins.
    #[serde(default)]
    pub agent_token: Option<String>,
    /// PEM of the daemon certificate, the only trust anchor for the agent.
    #[serde(default)]
    pub agent_cert: Option<PathBuf>,
    #[serde(default = "default_ffmpeg")]
    pub local_ffmpeg: PathBuf,
    #[serde(default = "default_ffprobe")]
    pub local_ffprobe: PathBuf,
    /// Seconds to wait for a worker slot. Jellyfin queues its own jobs, so a
    /// long wait beats running a transcode on a host with no GPU.
    #[serde(default = "default_queue_wait")]
    pub queue_wait_secs: u64,
    /// How long one process's "host down" verdict is trusted by the others.
    /// Expiry is the whole repair mechanism: no daemon clears the flag.
    #[serde(default = "default_down_ttl")]
    pub down_ttl_secs: u64,
    /// Name this client uses in the shared queue. Clients that point at the
    /// same `state_dir` must differ, or they count as one client.
    #[serde(default = "default_client_name")]
    pub client_name: String,
    /// Share of a busy worker, 1 to 1000. Read only while jobs wait: a client
    /// alone keeps taking every slot whatever its weight.
    #[serde(default = "default_client_weight")]
    pub client_weight: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            hosts: Vec::new(),
            stderr_as_stdout: false,
            state_dir: default_state_dir(),
            ssh_bin: default_ssh(),
            agent_token: None,
            agent_cert: None,
            local_ffmpeg: default_ffmpeg(),
            local_ffprobe: default_ffprobe(),
            queue_wait_secs: default_queue_wait(),
            down_ttl_secs: default_down_ttl(),
            client_name: default_client_name(),
            client_weight: default_client_weight(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Config> {
        let Some(path) = config_path() else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(Error::Io(e, format!("reading {}", path.display()))),
        }
    }

    pub fn from_toml(text: &str) -> Result<Config> {
        toml::from_str(text).map_err(|e| Error::Config(e.to_string()))
    }

    /// Local binary for the program, used when no worker can take the job.
    pub fn local_binary(&self, program: Program) -> &PathBuf {
        match program {
            Program::Ffmpeg => &self.local_ffmpeg,
            Program::Ffprobe => &self.local_ffprobe,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RRFFMPEG_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("rffmpeg/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_worker_with_capabilities_and_paths() {
        let cfg = Config::from_toml(
            r#"
stderr_as_stdout = true

[[hosts]]
name = "worker"
concurrency = 2
capabilities = ["vaapi"]

[[hosts.paths]]
from = "/home/alice/media"
to = "/home/bob/media"
"#,
        )
        .unwrap();
        assert!(cfg.stderr_as_stdout);
        assert_eq!(cfg.hosts.len(), 1);
        assert_eq!(cfg.hosts[0].concurrency, 2);
        assert_eq!(cfg.hosts[0].capabilities, vec!["vaapi".to_string()]);
        assert_eq!(cfg.hosts[0].paths[0].to, PathBuf::from("/home/bob/media"));
    }

    #[test]
    fn a_worker_without_limits_defaults_to_one_slot() {
        let cfg = Config::from_toml("[[hosts]]\nname = \"w\"\n").unwrap();
        assert_eq!(cfg.hosts[0].concurrency, 1);
        assert!(cfg.hosts[0].capabilities.is_empty());
    }

    #[test]
    fn an_empty_file_is_a_valid_local_only_config() {
        let cfg = Config::from_toml("").unwrap();
        assert!(cfg.hosts.is_empty());
        assert_eq!(cfg.local_ffmpeg, PathBuf::from("ffmpeg"));
    }
}
