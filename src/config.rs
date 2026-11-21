//! Worker list and paths, loaded from a TOML file.
//!
//! The file lives at `$RRFFMPEG_CONFIG`, else `$XDG_CONFIG_HOME/rffmpeg/config.toml`,
//! else `~/.config/rffmpeg/config.toml`. A missing file means "no workers", and
//! the dispatcher then runs ffmpeg locally.

use crate::{Class, Error, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn yes() -> bool {
    true
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

/// Every host key except the two caps.
///
/// A cap accepts a number or `"auto"`, which a flat struct cannot express, so
/// the shared keys are peeled off first and put back once the file is read
/// (ADR 0033). `deny_unknown_fields` lives here because the raw struct has to
/// flatten this one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostBase {
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub local: bool,
    #[serde(default)]
    pub ffmpeg_bin: Option<String>,
    #[serde(default)]
    pub ffprobe_bin: Option<String>,
    #[serde(default)]
    pub agent_addr: Option<String>,
    #[serde(default)]
    pub paths: Vec<PathMap>,
    #[serde(default)]
    pub reads: Vec<PathBuf>,
    #[serde(default)]
    pub prefer: i32,
    #[serde(default)]
    pub ssh_args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl HostBase {
    fn with_caps(self, concurrency: usize, light_concurrency: usize) -> Host {
        Host {
            name: self.name,
            concurrency,
            light_concurrency,
            capabilities: self.capabilities,
            local: self.local,
            ffmpeg_bin: self.ffmpeg_bin,
            ffprobe_bin: self.ffprobe_bin,
            agent_addr: self.agent_addr,
            paths: self.paths,
            reads: self.reads,
            prefer: self.prefer,
            ssh_args: self.ssh_args,
            env: self.env,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(from = "RawHost")]
pub struct Host {
    /// `ssh` destination: `worker`, `user@worker`, or an IP.
    pub name: String,
    /// Maximum jobs sent to this host at once. A plain number by the time any
    /// placement reads it: `"auto"` is resolved once, at startup, from this
    /// host's own journal (ADR 0033).
    pub concurrency: usize,
    /// Maximum Light jobs on this host at once: probes, thumbnails, trickplay
    /// scans. They never take a slot from `concurrency`, so a frame job does
    /// not wait behind a transcode.
    pub light_concurrency: usize,
    /// Advertised accelerators: `vaapi`, `nvenc`, `qsv`. Empty accepts any job.
    pub capabilities: Vec<String>,
    /// Run the jobs on this machine with `local_ffmpeg` and `local_ffprobe`,
    /// so the client's own GPU takes part in placement. `name` is then only a
    /// label: the journal, the locks and `--host` use it.
    pub local: bool,
    /// Remote ffmpeg command. Defaults to `ffmpeg` from the worker PATH; point
    /// it at a wrapper that sets the OpenCL environment.
    pub ffmpeg_bin: Option<String>,
    pub ffprobe_bin: Option<String>,
    /// `host:port` of the agent daemon. Set it to skip ssh for this host.
    pub agent_addr: Option<String>,
    pub paths: Vec<PathMap>,
    /// Roots this host can open, as the host sees them. A job whose input is
    /// under none of them is never placed here. Empty accepts any input, so a
    /// config written before this key routes as it did (ADR 0030).
    pub reads: Vec<PathBuf>,
    /// Placement preference among otherwise equal hosts. Higher wins, and the
    /// default is equal for every host (ADR 0031).
    pub prefer: i32,
    /// Extra ssh arguments for this host, appended after the global ones.
    pub ssh_args: Vec<String>,
    /// Variables set on the worker when it runs the job.
    pub env: BTreeMap<String, String>,
}

/// A cap as the file writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cap {
    /// Size from this host's own journal when the process starts.
    Auto,
    Fixed(usize),
}

/// What the file asked, before any journal is read. Kept per host, in file
/// order, so `Config::sized` knows which numbers it may replace.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Caps {
    pub heavy: Option<Cap>,
    pub light: Option<Cap>,
}

/// A cap as TOML gives it: an integer or a string.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawCap {
    Count(usize),
    Text(String),
}

fn cap_of(raw: RawCap, key: &str) -> std::result::Result<Cap, String> {
    match raw {
        RawCap::Count(0) => Err(refused(key)),
        RawCap::Count(count) => Ok(Cap::Fixed(count)),
        RawCap::Text(text) if text == "auto" => Ok(Cap::Auto),
        RawCap::Text(_) => Err(refused(key)),
    }
}

/// The one reason both refusals share: it is the operator's stake, not ours.
fn refused(key: &str) -> String {
    format!(
        "{key} must be a number or \"auto\": there is no unlimited pool, because it admits jobs the GPU cannot serve, and the bill lands on a live viewer's stream"
    )
}

/// One key's cap, with the key name kept for the error message. A field
/// attribute cannot pass the name down to a visitor, so each key is a type.
struct Heavy(Cap);
struct Light(Cap);

impl<'de> Deserialize<'de> for Heavy {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Heavy, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        cap_of(RawCap::deserialize(deserializer)?, "concurrency")
            .map(Heavy)
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for Light {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Light, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        cap_of(RawCap::deserialize(deserializer)?, "light_concurrency")
            .map(Light)
            .map_err(serde::de::Error::custom)
    }
}

/// The host as the file holds it: the shared keys, then the two caps.
#[derive(Deserialize)]
struct RawHost {
    #[serde(flatten)]
    base: HostBase,
    #[serde(default)]
    concurrency: Option<Heavy>,
    #[serde(default)]
    light_concurrency: Option<Light>,
}

impl RawHost {
    fn caps(&self) -> Caps {
        Caps {
            heavy: self.concurrency.as_ref().map(|Heavy(cap)| *cap),
            light: self.light_concurrency.as_ref().map(|Light(cap)| *cap),
        }
    }
}

impl From<RawHost> for Host {
    /// The numbers the file states, or the default this key had before
    /// `"auto"`: `Config::sized` replaces the ones left to the journal, and a
    /// `Host` built by hand keeps exactly what it holds.
    fn from(raw: RawHost) -> Host {
        let concurrency = match raw.concurrency {
            Some(Heavy(Cap::Fixed(count))) => count,
            Some(Heavy(Cap::Auto)) | None => 1,
        };
        let light_concurrency = match raw.light_concurrency {
            Some(Light(Cap::Fixed(count))) => count,
            Some(Light(Cap::Auto)) => crate::sizing::light_cap(cpus()),
            None => 2,
        };
        raw.base.with_caps(concurrency, light_concurrency)
    }
}

/// The caps on their own. `Host` has to stay flat for the code that builds
/// it, so the text is read twice: once for the hosts, once for what they asked
/// about their two caps.
#[derive(Deserialize)]
struct RawHosts {
    #[serde(default)]
    hosts: Vec<RawHost>,
}

/// Cores this machine reports. `available_parallelism` also honours a cgroup
/// quota, which is the number that matters in a container.
pub(crate) fn cpus() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
}

impl Host {
    /// Slots in the pool this class runs in. A Light job has its own pool, so a
    /// full encoder never costs a probe a wait.
    pub fn slots(&self, class: Class) -> usize {
        match class {
            Class::Light => self.light_concurrency,
            Class::Heavy => self.concurrency,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<Host>,
    /// What each host asked about its two caps, in the same order as `hosts`.
    /// Skipped by serde because `Config::from_toml` fills it from a second
    /// read; a `Config` built by hand keeps every cap it holds.
    #[serde(skip)]
    pub(crate) caps: Vec<Caps>,
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
            caps: Vec::new(),
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
        let mut config: Config = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        let asked: RawHosts = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        config.caps = asked.hosts.iter().map(RawHost::caps).collect();
        let mut seen = std::collections::HashSet::new();
        let mut local_seen: Option<&str> = None;
        for host in &config.hosts {
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

    /// Replace every cap left to `"auto"` with a number read from that host's
    /// own journal, and return the audit lines to append once the journal is
    /// open.
    ///
    /// Nothing here writes state: two processes that read the same tail
    /// compute the same caps, so a stored number could only disagree with
    /// them (ADR 0033). A host that is not `local` is never sized from this
    /// machine's numbers, which would export a guess across a network.
    pub fn sized(mut self) -> (Config, Vec<crate::state::Event>) {
        let cpus = cpus();
        let records = crate::state::tail_records(&self.state_dir);
        let samples = crate::sizing::samples(&records);
        let asked = self.caps.clone();
        let mut events = Vec::new();
        for (index, host) in self.hosts.iter_mut().enumerate() {
            // No request means a `Config` built in code: it says what it
            // means, and nothing may overwrite it.
            let Some(caps) = asked.get(index).copied() else {
                continue;
            };
            if caps.heavy.is_none() || caps.light.is_none() {
                // One line at startup, so an operator sees which host is
                // sized by a rule instead of by a number they wrote.
                let what = match (caps.heavy.is_none(), caps.light.is_none()) {
                    (true, true) => "concurrency and light_concurrency",
                    (true, false) => "concurrency",
                    _ => "light_concurrency",
                };
                let why = if !host.local {
                    format!(
                        "it keeps the default: {} heavy, {} light",
                        host.concurrency, host.light_concurrency
                    )
                } else if caps.heavy.is_none() {
                    "the pools are sized from its journal".to_string()
                } else {
                    "the light pool follows this machine's core count".to_string()
                };
                crate::log_at!(
                    crate::log::Level::Warn,
                    "host {}: {} left out, so {}",
                    host.name,
                    what,
                    why
                );
            }
            if caps.light.is_none() && host.local {
                host.light_concurrency = crate::sizing::light_cap(cpus);
            }
            let auto = match caps.heavy {
                Some(Cap::Auto) => true,
                Some(Cap::Fixed(_)) => false,
                None => host.local,
            };
            if !auto {
                continue;
            }
            let tail = samples.get(&host.name).map(Vec::as_slice).unwrap_or(&[]);
            let size = crate::sizing::heavy(tail, cpus);
            host.concurrency = size.cap;
            let class = size
                .key
                .as_ref()
                .map_or_else(|| "heavy".to_string(), |key| key.label());
            if size.key.is_none() {
                crate::log_at!(
                    crate::log::Level::Warn,
                    "host {}: no finished transcode of this class yet, heavy cap 1",
                    host.name
                );
            }
            let previous = crate::sizing::last_sized(&records, &host.name, &class);
            if previous != size.cap {
                events.push(crate::state::Event::Sized {
                    host: host.name.clone(),
                    class,
                    cap: size.cap,
                    samples: size.samples,
                    p05: size.p05,
                    previous,
                });
            }
        }
        (self, events)
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
        assert_eq!(cfg.hosts[0].light_concurrency, 2);
        assert!(cfg.hosts[0].capabilities.is_empty());
    }

    /// The light pool is the point of the split, so a host that names no
    /// `light_concurrency` still gets one.
    #[test]
    fn the_light_pool_defaults_to_two_and_can_be_set() {
        let cfg = Config::from_toml("[[hosts]]\nname = \"w\"\nlight_concurrency = 4\n").unwrap();
        assert_eq!(cfg.hosts[0].slots(crate::Class::Light), 4);
        assert_eq!(cfg.hosts[0].slots(crate::Class::Heavy), 1);
        let err =
            Config::from_toml("[[hosts]]\nname = \"w\"\nlight_concurrency = 0\n").unwrap_err();
        assert!(
            matches!(err, Error::Config(ref m) if m.contains("light_concurrency")),
            "{err}"
        );
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
        let first = queue.join("worker", Class::Heavy, 0, "alice", 0);
        let second = queue.join("worker", Class::Heavy, 0, "bob", 0);
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

    /// A state dir of this test's own, holding the journal lines given.
    fn journal_dir(tag: &str, lines: &[String]) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("rrffmpeg-config-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("journal"), lines.join("\n") + "\n").unwrap();
        dir
    }

    /// One transcode: a start line, then a finish line carrying its speed.
    fn transcode(seq: u64, job: &str, host: &str, speed: f64) -> [String; 2] {
        [
            format!(
                r#"{{"seq":{seq},"ts":1000,"ev":{{"kind":"started","job":"{job}","host":"{host}","in_flight":1,"class":"heavy","job_kind":"transcode","input":"/m/x.mkv"}}}}"#
            ),
            format!(
                r#"{{"seq":{},"ts":121000,"ev":{{"kind":"finished","job":"{job}","code":0,"host":"{host}","dur_us":120000000,"speed":{speed},"src_codec":"hevc","src_h":1080,"dst_codec":"h264","video":"encode"}}}}"#,
                seq + 1
            ),
        ]
    }

    #[test]
    fn a_cap_accepts_a_number_or_auto_and_refuses_unlimited() {
        let cfg = Config::from_toml(
            "[[hosts]]\nname = \"w\"\nconcurrency = \"auto\"\nlight_concurrency = 4\n",
        )
        .unwrap();
        assert_eq!(cfg.caps[0].heavy, Some(Cap::Auto));
        assert_eq!(cfg.caps[0].light, Some(Cap::Fixed(4)));
        // Absent means auto, which the next test resolves: here the file said
        // nothing at all.
        let plain = Config::from_toml("[[hosts]]\nname = \"w\"\n").unwrap();
        assert_eq!(plain.caps[0].heavy, None);
        for text in [
            "[[hosts]]\nname = \"w\"\nconcurrency = 0\n",
            "[[hosts]]\nname = \"w\"\nconcurrency = \"unlimited\"\n",
            "[[hosts]]\nname = \"w\"\nlight_concurrency = 0\n",
            "[[hosts]]\nname = \"w\"\nlight_concurrency = \"none\"\n",
        ] {
            let err = Config::from_toml(text).unwrap_err();
            let text = err.to_string();
            assert!(
                matches!(err, Error::Config(ref t)
                    if t.contains("number or \"auto\"") && t.contains("viewer")),
                "{text}"
            );
        }
    }

    #[test]
    fn auto_sizes_a_local_host_from_its_own_journal() {
        let lines: Vec<String> = transcode(1, "a", "here", 2.6).into_iter().collect();
        let dir = journal_dir("sized", &lines);
        let text = format!(
            "state_dir = {:?}\n[[hosts]]\nname = \"here\"\nlocal = true\nconcurrency = \"auto\"\n",
            dir
        );
        let (cfg, events) = Config::from_toml(&text).unwrap().sized();
        let want = crate::sizing::ceiling(cpus()).min(2);
        assert_eq!(cfg.hosts[0].concurrency, want, "floor(2.6 / 1.3) is 2");
        assert_eq!(
            cfg.hosts[0].light_concurrency,
            crate::sizing::light_cap(cpus())
        );
        let sized = events
            .iter()
            .find_map(|event| match event {
                crate::state::Event::Sized {
                    cap,
                    samples,
                    previous,
                    ..
                } => Some((*cap, *samples, *previous)),
                _ => None,
            })
            .expect("a Sized audit line");
        assert_eq!(sized, (want, 1, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_host_left_out_is_sized_from_its_journal_and_a_worker_defaults_to_one() {
        let lines: Vec<String> = transcode(1, "a", "here", 2.6).into_iter().collect();
        let dir = journal_dir("absent", &lines);
        let text = format!(
            "state_dir = {:?}\n[[hosts]]\nname = \"here\"\nlocal = true\n\n[[hosts]]\nname = \"worker\"\n",
            dir
        );
        let (cfg, _) = Config::from_toml(&text).unwrap().sized();
        assert_eq!(
            cfg.hosts[0].concurrency,
            crate::sizing::ceiling(cpus()).min(2)
        );
        assert_eq!(
            cfg.hosts[1].concurrency, 1,
            "a remote host is never sized from this machine"
        );
        assert_eq!(cfg.hosts[1].light_concurrency, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_journal_sizes_to_one_job() {
        // Before 0.13 the journal had no class, no kind and no speed. A tail
        // of those lines must size to one, not fail and not guess.
        let old = r#"{"seq":1,"ts":1000,"ev":{"kind":"started","job":"a","host":"here","elapsed_us":900}}"#;
        let old_finish = r#"{"seq":2,"ts":121000,"ev":{"kind":"finished","job":"a","code":0,"host":"here","dur_us":120000}}"#;
        let dir = journal_dir("old", &[old.to_string(), old_finish.to_string()]);
        let text = format!(
            "state_dir = {:?}\n[[hosts]]\nname = \"here\"\nlocal = true\n",
            dir
        );
        let (cfg, _) = Config::from_toml(&text).unwrap().sized();
        assert_eq!(cfg.hosts[0].concurrency, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
