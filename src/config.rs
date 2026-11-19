//! Worker list and paths, loaded from a TOML file.
//!
//! The file lives at `$RRFFMPEG_CONFIG`, else `$XDG_CONFIG_HOME/rffmpeg/config.toml`,
//! else `~/.config/rffmpeg/config.toml`. A missing file means "no workers", and
//! the dispatcher then runs ffmpeg locally.

use crate::{Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn yes() -> bool {
    true
}

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

/// Keep the ssh connection to a host open this long between jobs. 300 matches
/// rffmpeg, so a job opens a channel instead of paying a handshake.
fn default_ssh_persist() -> u64 {
    300
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
#[serde(deny_unknown_fields)]
pub struct PathMap {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// `ssh` destination: `worker`, `user@worker`, or an IP.
    pub name: String,
    /// Maximum jobs sent to this host at once.
    #[serde(default = "one")]
    pub concurrency: usize,
    /// Advertised accelerators: `vaapi`, `nvenc`, `qsv`. Empty accepts any job.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Run the jobs on this machine with `local_ffmpeg` and `local_ffprobe`,
    /// so the client's own GPU takes part in placement. `name` is then only a
    /// label: the journal, the locks and `--host` use it.
    #[serde(default)]
    pub local: bool,
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
    /// Roots this host can open, as the host sees them. A job whose input is
    /// under none of them is never placed here. Empty accepts any input, so a
    /// config written before this key routes as it did (ADR 0030).
    #[serde(default)]
    pub reads: Vec<PathBuf>,
    /// Placement preference among otherwise equal hosts. Higher wins, and the
    /// default is equal for every host (ADR 0031).
    #[serde(default)]
    pub prefer: i32,
    /// Extra ssh arguments for this host, appended after the global ones.
    #[serde(default)]
    pub ssh_args: Vec<String>,
    /// Variables set on the worker when it runs the job.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Seconds to keep one ssh connection per host open between jobs. Zero
    /// spawns a fresh ssh, and pays the handshake every time.
    #[serde(default = "default_ssh_persist")]
    pub ssh_persist_secs: u64,
    /// Where the control sockets go. Defaults to a tmpfs location, see
    /// `docs/latency.md`. Only used when `ssh_persist_secs` is above zero.
    #[serde(default)]
    pub ssh_socket_dir: Option<PathBuf>,
    /// Extra ssh arguments, shared by every host. A host may append more.
    #[serde(default)]
    pub ssh_args: Vec<String>,
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
    /// Run the local binary when no worker slot frees up. An ffprobe always
    /// does, a transcode only when this is true: software encoding on a
    /// GPU-less client is worse than a failed request.
    #[serde(default = "yes")]
    pub fallback_local: bool,
    /// Variables set on the local binary. A GPU-less client uses none.
    #[serde(default)]
    pub local_env: BTreeMap<String, String>,
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
            ssh_persist_secs: default_ssh_persist(),
            ssh_socket_dir: None,
            ssh_args: Vec::new(),
            agent_token: None,
            agent_cert: None,
            local_ffmpeg: default_ffmpeg(),
            local_ffprobe: default_ffprobe(),
            queue_wait_secs: default_queue_wait(),
            fallback_local: true,
            local_env: BTreeMap::new(),
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
        let config: Config = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let mut seen = std::collections::HashSet::new();
        let mut local_seen: Option<&str> = None;
        for host in &config.hosts {
            if host.concurrency == 0 {
                return Err(Error::Config(format!(
                    "host {}: concurrency must be at least 1",
                    host.name
                )));
            }
            if !seen.insert(host.name.as_str()) {
                return Err(Error::Config(format!("duplicate host name {}", host.name)));
            }
            // A relative root matches no absolute input, so the host would
            // silently take no job at all.
            for root in &host.reads {
                if !root.is_absolute() {
                    return Err(Error::Config(format!(
                        "host {}: reads {} must be an absolute path",
                        host.name,
                        root.display()
                    )));
                }
            }
            if host.local {
                // Each of these would be silently ignored, so refuse instead.
                if host.agent_addr.is_some() {
                    return Err(Error::Config(format!(
                        "host {}: local and agent_addr exclude each other",
                        host.name
                    )));
                }
                if !host.paths.is_empty() {
                    return Err(Error::Config(format!(
                        "host {}: a local host reads the client's own paths, so it needs no path map",
                        host.name
                    )));
                }
                if host.ffmpeg_bin.is_some() || host.ffprobe_bin.is_some() {
                    return Err(Error::Config(format!(
                        "host {}: a local host runs local_ffmpeg and local_ffprobe",
                        host.name
                    )));
                }
                if !host.ssh_args.is_empty() {
                    return Err(Error::Config(format!(
                        "host {}: a local host takes no ssh_args",
                        host.name
                    )));
                }
                // Slots are exclusive per host name, so two local entries would
                // each allow `concurrency` jobs on one GPU.
                if let Some(first) = local_seen {
                    return Err(Error::Config(format!(
                        "host {}: {first} already runs the jobs here, and one machine has one set of slots",
                        host.name
                    )));
                }
                local_seen = Some(&host.name);
            }
        }
        Ok(config)
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
    fn a_second_local_host_is_refused() {
        let err = Config::from_toml(
            "[[hosts]]\nname = \"a\"\nlocal = true\n\n[[hosts]]\nname = \"b\"\nlocal = true\n",
        )
        .expect_err("two local hosts");
        assert!(
            matches!(&err, Error::Config(text) if text.contains("one set of slots")),
            "{err:?}"
        );
    }

    #[test]
    fn a_worker_without_limits_defaults_to_one_slot() {
        let cfg = Config::from_toml("[[hosts]]\nname = \"w\"\n").unwrap();
        assert_eq!(cfg.hosts[0].concurrency, 1);
        assert!(cfg.hosts[0].capabilities.is_empty());
    }

    #[test]
    fn ssh_persistence_defaults_on_and_can_be_turned_off() {
        let cfg = Config::from_toml("").unwrap();
        assert_eq!(cfg.ssh_persist_secs, 300);
        assert_eq!(cfg.ssh_socket_dir, None);
        let off = Config::from_toml("ssh_persist_secs = 0").unwrap();
        assert_eq!(off.ssh_persist_secs, 0);
        let pinned = Config::from_toml("ssh_socket_dir = \"/run/user/1000/rffmpeg\"").unwrap();
        assert_eq!(
            pinned.ssh_socket_dir,
            Some(PathBuf::from("/run/user/1000/rffmpeg"))
        );
    }

    #[test]
    fn the_fallback_is_on_by_default_and_can_be_forbidden() {
        assert!(Config::from_toml("").unwrap().fallback_local);
        assert!(
            !Config::from_toml("fallback_local = false")
                .unwrap()
                .fallback_local
        );
    }

    #[test]
    fn ssh_args_and_env_are_empty_by_default_and_add_up_per_host() {
        let cfg = Config::from_toml(
            r#"
ssh_args = ["-o", "StrictHostKeyChecking=yes"]

[[hosts]]
name = "worker"
ssh_args = ["-i", "/home/alice/.ssh/worker"]

[hosts.env]
OCL_ICD_VENDORS = "/opt/intel/opencl"
"#,
        )
        .unwrap();
        assert_eq!(cfg.ssh_args, vec!["-o", "StrictHostKeyChecking=yes"]);
        assert_eq!(cfg.hosts[0].ssh_args, vec!["-i", "/home/alice/.ssh/worker"]);
        assert_eq!(
            cfg.hosts[0].env.get("OCL_ICD_VENDORS").map(String::as_str),
            Some("/opt/intel/opencl")
        );
        assert!(Config::from_toml("").unwrap().local_env.is_empty());
        assert!(Config::from_toml("").unwrap().ssh_args.is_empty());
    }

    #[test]
    fn a_bad_ssh_arg_type_is_a_config_error() {
        let err = Config::from_toml("ssh_args = \"-i key\"").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
    }

    #[test]
    fn an_empty_file_is_a_valid_local_only_config() {
        let cfg = Config::from_toml("").unwrap();
        assert!(cfg.hosts.is_empty());
        assert_eq!(cfg.local_ffmpeg, PathBuf::from("ffmpeg"));
    }

    #[test]
    fn rrffmpeg_config_env_wins_over_the_default_path() {
        let _guard = crate::exclusive();
        let prev_config = std::env::var_os("RRFFMPEG_CONFIG");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: the exclusive lock serialises this test, and both values are
        // restored before it returns.
        unsafe {
            std::env::set_var("RRFFMPEG_CONFIG", "/home/alice/custom.toml");
            std::env::set_var("XDG_CONFIG_HOME", "/home/alice/.config");
        }
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/home/alice/custom.toml"))
        );
        match prev_config {
            Some(value) => unsafe { std::env::set_var("RRFFMPEG_CONFIG", value) },
            None => unsafe { std::env::remove_var("RRFFMPEG_CONFIG") },
        }
        match prev_xdg {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }

    #[test]
    fn a_host_with_zero_slots_is_a_config_error() {
        let err = Config::from_toml("[[hosts]]\nname = \"worker\"\nconcurrency = 0\n").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
    }

    #[test]
    fn duplicate_host_names_are_rejected() {
        let err =
            Config::from_toml("[[hosts]]\nname = \"worker\"\n\n[[hosts]]\nname = \"worker\"\n")
                .unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
    }

    #[test]
    fn a_local_host_parses_and_keeps_its_label_and_env() {
        let cfg = Config::from_toml(
            r#"
[[hosts]]
name = "client"
local = true
concurrency = 2
capabilities = ["vaapi"]

[hosts.env]
LD_LIBRARY_PATH = "/opt/driver/lib"
"#,
        )
        .unwrap();
        assert!(cfg.hosts[0].local);
        assert_eq!(cfg.hosts[0].name, "client");
        assert_eq!(cfg.hosts[0].concurrency, 2);
        assert_eq!(
            cfg.hosts[0].env.get("LD_LIBRARY_PATH").map(String::as_str),
            Some("/opt/driver/lib")
        );
    }

    #[test]
    fn a_local_host_refuses_keys_it_would_ignore() {
        for body in [
            "local = true\nagent_addr = \"worker.invalid:9876\"",
            "local = true\nffmpeg_bin = \"/usr/bin/ffmpeg\"",
            "local = true\nffprobe_bin = \"/usr/bin/ffprobe\"",
            "local = true\nssh_args = [\"-p\", \"22\"]",
        ] {
            let text = format!("[[hosts]]\nname = \"client\"\n{body}\n");
            let err = Config::from_toml(&text).unwrap_err();
            assert!(matches!(err, Error::Config(_)), "{err}");
        }
        let mapped = "[[hosts]]\nname = \"client\"\nlocal = true\n\n[[hosts.paths]]\nfrom = \"/home/alice\"\nto = \"/home/bob\"\n";
        let err = Config::from_toml(mapped).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
    }

    #[test]
    fn client_weight_zero_does_not_divide_by_zero() {
        let cfg = Config::from_toml("client_weight = 0").unwrap();
        assert_eq!(cfg.client_weight, 0);
        // The queue clamps the weight, so two waiters still order with no panic.
        let dir =
            std::env::temp_dir().join(format!("rrffmpeg-config-weight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let queue = crate::queue::Queue::new(&dir);
        let first = queue.join("worker", 0, "alice", 0);
        let second = queue.join("worker", 0, "bob", 0);
        assert!(
            queue.ahead(&first) ^ queue.ahead(&second),
            "exactly one of two waiters must rank first"
        );
        drop((first, second));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_relative_reads_entry_is_a_config_error() {
        let err = Config::from_toml("[[hosts]]\nname = \"w\"\nreads = [\"media\"]\n").unwrap_err();
        let text = err.to_string();
        assert!(
            matches!(err, Error::Config(ref t) if t.contains("absolute")),
            "{text}"
        );
    }

    #[test]
    fn reads_and_prefer_default_to_accept_anything_and_no_preference() {
        let cfg = Config::from_toml("[[hosts]]\nname = \"w\"\n").unwrap();
        assert!(cfg.hosts[0].reads.is_empty());
        assert_eq!(cfg.hosts[0].prefer, 0);
        let set =
            Config::from_toml("[[hosts]]\nname = \"w\"\nreads = [\"/mnt/archive\"]\nprefer = 10\n")
                .unwrap();
        assert_eq!(set.hosts[0].reads, vec![PathBuf::from("/mnt/archive")]);
        assert_eq!(set.hosts[0].prefer, 10);
    }

    #[test]
    fn an_unknown_key_is_reported_not_ignored() {
        let err = Config::from_toml("client_weightt = 100").unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
        // The shipped example must stay valid under deny_unknown_fields.
        Config::from_toml(include_str!("../config.example.toml")).expect("example config");
    }
}
