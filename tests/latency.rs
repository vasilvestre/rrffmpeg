//! Latency measurements for the dispatcher, on loopback.
//!
//! These are measurements, not assertions: they print numbers and never fail.
//! Run them by hand, and keep the machine and the load in the report.
//!
//! ```text
//! cargo test --release --test latency -- --ignored --nocapture
//! ```

use rrffmpeg::agent::{Agent, generate_cert, serve_on, server_config};
use rrffmpeg::janitor::Janitor;
use rrffmpeg::locks::LockDir;
use rrffmpeg::scheduler::Scheduler;
use rrffmpeg::state::{Event, Journal, Record};
use rrffmpeg::transport::Transport;
use rrffmpeg::{Host, Job, Program};
use std::ffi::OsString;
use std::io::{BufWriter, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const TOKEN: &str = "latency-token";

/// The reference point: a loopback box, no GPU, no ssh. The TLS certificate
/// lives in a temp dir for the duration of the run.
fn setup() -> (Arc<rrffmpeg::config::Host>, Agent, PathBuf) {
    let dir = std::env::temp_dir().join(format!("rrffmpeg-latency-{}", std::process::id()));
    let sans = vec!["127.0.0.1".to_string()];
    generate_cert(&dir, &sans).expect("cert");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    let tls = server_config(&dir.join("cert.pem"), &dir.join("key.pem")).expect("tls");
    std::thread::spawn(move || {
        let _ = serve_on(listener, TOKEN.to_string(), tls);
    });

    let host = rrffmpeg::config::Host {
        name: "loopback".into(),
        concurrency: 1,
        capabilities: vec![],
        local: false,
        // /bin/true keeps the measurement on the dispatcher: no codec work.
        ffmpeg_bin: Some("/bin/true".into()),
        ffprobe_bin: Some("/bin/true".into()),
        agent_addr: Some(addr),
        ssh_args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        paths: vec![],
        reads: Vec::new(),
        prefer: 0,
    };
    let agent = Agent::load(TOKEN.into(), &dir.join("cert.pem")).expect("agent");
    (Arc::new(host), agent, dir)
}

fn job(id: u64) -> Job {
    Job {
        id: format!("lat-{id}"),
        program: Program::Ffmpeg,
        args: Vec::<OsString>::new(),
        priority: 0,
        capabilities: Vec::new(),
    }
}

fn percentiles(samples: &mut [Duration]) -> (f64, f64, f64, f64) {
    samples.sort_unstable();
    let at = |p: f64| {
        let n = samples.len();
        let idx = ((p * n as f64).ceil() as usize)
            .saturating_sub(1)
            .min(n - 1);
        samples[idx].as_secs_f64() * 1000.0
    };
    (at(0.50), at(0.95), at(0.99), at(1.0))
}

fn report(name: &str, samples: &mut [Duration]) {
    let (p50, p95, p99, max) = percentiles(samples);
    println!(
        "{name:<34} n={:<4} p50={p50:7.2} ms  p95={p95:7.2} ms  p99={p99:7.2} ms  max={max:7.2} ms",
        samples.len()
    );
    std::io::stdout().flush().ok();
}

fn main_note() {
    println!(
        "loopback, {} cores, {} MiB RAM, load {}",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| m.lines().next().map(|l| l.to_string()))
            .and_then(|l| l.split_whitespace().nth(1).map(|k| k.parse::<u64>().ok()))
            .flatten()
            .map(|kib| kib / 1024)
            .unwrap_or(0),
        std::fs::read_to_string("/proc/loadavg")
            .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    );
}

#[test]
#[ignore = "measurement: cargo test --release --test latency -- --ignored --nocapture"]
fn dispatch_latency() {
    const N: u64 = 200;
    let (host, agent, dir) = setup();
    main_note();

    // Warm both sides: the first connection pays page faults and key loading.
    for i in 0..10 {
        let _ = agent.run(&host, &job(i), false);
    }

    // One status round trip: TCP connect, TLS handshake, hello, reply, close.
    let mut probes = Vec::with_capacity(N as usize);
    for _ in 0..N {
        let start = Instant::now();
        let ok = agent.probe(&host);
        probes.push(start.elapsed());
        assert!(ok, "probe failed");
    }
    report("probe (one TLS connection)", &mut probes);

    // A whole job: two connections, two handshakes, spawn, frames, exit.
    let mut runs = Vec::with_capacity(N as usize);
    for i in 0..N {
        let start = Instant::now();
        let code = agent.run(&host, &job(i), false).expect("run");
        runs.push(start.elapsed());
        assert_eq!(code, 0);
    }
    report("dispatch through the agent", &mut runs);

    // Floor: starting the same process locally, no transport at all.
    let mut local = Vec::with_capacity(N as usize);
    for _ in 0..N {
        let start = Instant::now();
        let status = std::process::Command::new("/bin/true")
            .status()
            .expect("spawn");
        local.push(start.elapsed());
        assert!(status.success());
    }
    report("local /bin/true (floor)", &mut local);

    std::fs::remove_dir_all(&dir).ok();
    println!();
    println!("delta = dispatch - floor = the cost the transport adds per job");
}

#[test]
#[ignore = "measurement: cargo test --release --test latency -- --ignored --nocapture dispatch_latency_ssh"]
fn dispatch_latency_ssh() {
    const N: u64 = 200;
    // No agent_addr, so the Ssh transport is the one under test. /bin/true keeps
    // the measurement on the dispatcher: no codec work on the worker.
    let host = Arc::new(Host {
        name: "localhost".into(),
        concurrency: 1,
        capabilities: vec![],
        local: false,
        ffmpeg_bin: Some("/bin/true".into()),
        ffprobe_bin: Some("/bin/true".into()),
        agent_addr: None,
        ssh_args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        paths: vec![],
        reads: Vec::new(),
        prefer: 0,
    });
    let ssh = rrffmpeg::transport::Ssh::new(PathBuf::from("ssh"));
    main_note();

    // Fail loudly on a missing sshd instead of printing zeros for a dead path.
    assert!(
        ssh.probe(&host),
        "ssh to localhost is not usable, start sshd and add the key"
    );

    // Warm the connection path: the first ssh pays page faults and key loading.
    for i in 0..10 {
        let _ = ssh.run(&host, &job(i), false);
    }

    // One status check: spawn, TCP connect, key exchange, auth, remote `true`.
    let mut probes = Vec::with_capacity(N as usize);
    for _ in 0..N {
        let start = Instant::now();
        let ok = ssh.probe(&host);
        probes.push(start.elapsed());
        assert!(ok, "probe failed");
    }
    report("probe (one ssh connection)", &mut probes);

    // A whole job: one ssh process, one connection, spawn, exit.
    let mut runs = Vec::with_capacity(N as usize);
    for i in 0..N {
        let start = Instant::now();
        let code = ssh.run(&host, &job(i), false).expect("run");
        runs.push(start.elapsed());
        assert_eq!(code, 0);
    }
    report("dispatch through ssh", &mut runs);

    // Floor: starting the same process locally, no transport at all.
    let mut local = Vec::with_capacity(N as usize);
    for _ in 0..N {
        let start = Instant::now();
        let status = std::process::Command::new("/bin/true")
            .status()
            .expect("spawn");
        local.push(start.elapsed());
        assert!(status.success());
    }
    report("local /bin/true (floor)", &mut local);

    println!();
    println!("delta = dispatch - floor = the cost the transport adds per job");
}

/// RSS in KiB, thread count, and open fd count for this process.
///
/// The daemon runs in-process, so these include the test harness. Only the
/// delta across the run is meaningful.
fn snapshot() -> (u64, u64, usize) {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let field = |key: &str| {
        status
            .lines()
            .find(|line| line.starts_with(key))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    let fds = std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count())
        .unwrap_or(0);
    (field("VmRSS:"), field("Threads:"), fds)
}

#[test]
#[ignore = "measurement: cargo test --release --test latency -- --ignored --nocapture job_resources"]
fn job_resources() {
    const N: u64 = 500;
    let (host, agent, dir) = setup();
    main_note();

    // Warm the TLS path and the daemon's threads before the baseline.
    for i in 0..10 {
        agent.run(&host, &job(i), false).expect("warm run");
    }
    let before = snapshot();
    for i in 0..N {
        let code = agent.run(&host, &job(i), false).expect("run");
        assert_eq!(code, 0);
    }
    let after = snapshot();

    println!("job_resources: {N} jobs through the in-process agent daemon");
    for (label, (rss, threads, fds)) in [("before", before), ("after", after)] {
        println!("{label:<7} RSS {rss:>8} KiB  threads {threads:>4}  fds {fds:>4}");
    }
    let delta = |a: u64, b: u64| a as f64 - b as f64;
    println!(
        "per job dRSS {:+.2} KiB  dthreads {:+.2}  dfds {:+.2}",
        delta(after.0, before.0) / N as f64,
        delta(after.1, before.1) / N as f64,
        delta(after.2 as u64, before.2 as u64) / N as f64
    );
    std::io::stdout().flush().ok();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "measurement: cargo test --release --test latency -- --ignored --nocapture journal_replay_at_rotation_cap"]
fn journal_replay_at_rotation_cap() {
    // The cap comes from ROTATE_BYTES in src/state.rs: 8 MiB. A line is about
    // 100 bytes, so the cap sets the record count, not the other way round.
    const CAP: u64 = 8 * 1024 * 1024;
    let dir = std::env::temp_dir().join(format!("rrffmpeg-latency-journal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");

    let mut file = BufWriter::new(std::fs::File::create(dir.join("journal")).expect("create"));
    let mut bytes = 0u64;
    let mut records = 0u64;
    while bytes < CAP {
        records += 1;
        let id = format!("{}-{}", 31_000 + records % 97, records);
        let ev = if records.is_multiple_of(2) {
            Event::Finished {
                job: id,
                code: 0,
                host: "worker".into(),
                dur_us: 1000,
                stats: Box::default(),
            }
        } else {
            Event::Started {
                job: id,
                host: "worker".into(),
                elapsed_us: 400 + records % 900,
                program: "ffmpeg".into(),
                in_flight: 1,
            }
        };
        let line = serde_json::to_string(&Record {
            seq: records,
            ts: 1_763_300_000_000_000 + records * 137,
            ev,
        })
        .expect("record json");
        writeln!(file, "{line}").expect("write line");
        bytes += line.len() as u64 + 1;
    }
    file.flush().expect("flush");
    drop(file);

    let size = std::fs::metadata(dir.join("journal"))
        .expect("stat journal")
        .len();
    println!("journal_replay_at_rotation_cap: {records} records, {size} B, cap {CAP} B");

    let mut samples = Vec::with_capacity(5);
    for _ in 0..5 {
        let began = Instant::now();
        let journal = Journal::open(&dir).expect("open journal");
        samples.push(began.elapsed());
        drop(journal);
    }
    for (index, sample) in samples.iter().enumerate() {
        println!("run {index}: {:>8.3} ms", sample.as_secs_f64() * 1000.0);
    }
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let median = sorted[2].as_secs_f64() * 1000.0;
    println!(
        "median {median:.3} ms, {:.3} us per record",
        median * 1000.0 / records as f64
    );
    std::io::stdout().flush().ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// A host that answers `probe` from a flag, so a test can clear a fault.
struct ProbeToggle {
    up: AtomicBool,
    probes: AtomicUsize,
}

impl Transport for ProbeToggle {
    fn run(&self, _host: &Host, _job: &Job, _stderr_as_stdout: bool) -> rrffmpeg::Result<i32> {
        Err(rrffmpeg::Error::Transport("probe toggle".into()))
    }

    fn probe(&self, _host: &Host) -> bool {
        self.probes.fetch_add(1, Ordering::SeqCst);
        self.up.load(Ordering::SeqCst)
    }

    fn job_alive(&self, _host: &Host, _job_id: &str) -> bool {
        false
    }
}

fn worker() -> Host {
    Host {
        name: "worker".into(),
        concurrency: 1,
        capabilities: Vec::new(),
        local: false,
        ffmpeg_bin: None,
        ffprobe_bin: None,
        agent_addr: None,
        ssh_args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        paths: Vec::new(),
        reads: Vec::new(),
        prefer: 0,
    }
}

/// Poll until `cond` holds or the deadline passes. Never sleeps blindly.
fn wait_for(mut cond: impl FnMut() -> bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

fn janitor_dir(tag: &str, run: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rrffmpeg-latency-janitor-{}-{run}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn janitor_wiring(dir: &Path, up: bool) -> (Arc<Scheduler>, Arc<ProbeToggle>) {
    let scheduler = Arc::new(Scheduler::new(
        vec![worker()],
        Arc::new(LockDir::new(dir)),
        Duration::from_secs(60),
    ));
    scheduler.mark_down("worker");
    (
        scheduler,
        Arc::new(ProbeToggle {
            up: AtomicBool::new(up),
            probes: AtomicUsize::new(0),
        }),
    )
}

#[test]
#[ignore = "measurement: cargo test --release --test latency -- --ignored --nocapture janitor_recovery"]
fn janitor_recovery() {
    // The production interval, from src/main.rs.
    const EVERY: Duration = Duration::from_secs(10);
    let mut at_spawn = Vec::with_capacity(3);
    let mut after_clear = Vec::with_capacity(3);

    for run in 0..3 {
        // (a) Down to up: the very first probe already answers true.
        let dir = janitor_dir("spawn", run);
        let (scheduler, transport) = janitor_wiring(&dir, true);
        let journal = Arc::new(Journal::open(&dir).expect("journal"));
        let began = Instant::now();
        let janitor = Janitor::spawn(scheduler.clone(), journal, transport, EVERY);
        let recovered = wait_for(|| !scheduler.is_down("worker"), Duration::from_secs(15));
        at_spawn.push(began.elapsed());
        janitor.stop();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(recovered, "the host never came up at spawn in run {run}");

        // (b) Fault cleared: the janitor first sees the fault, then the flag flips.
        let dir = janitor_dir("clear", run);
        let (scheduler, transport) = janitor_wiring(&dir, false);
        let journal = Arc::new(Journal::open(&dir).expect("journal"));
        let probe: Arc<dyn Transport> = transport.clone();
        let janitor = Janitor::spawn(scheduler.clone(), journal, probe, EVERY);
        assert!(
            wait_for(
                || transport.probes.load(Ordering::SeqCst) >= 1,
                Duration::from_secs(5)
            ),
            "the janitor never probed in run {run}"
        );
        assert!(scheduler.is_down("worker"));
        let began = Instant::now();
        transport.up.store(true, Ordering::SeqCst);
        let recovered = wait_for(|| !scheduler.is_down("worker"), Duration::from_secs(15));
        after_clear.push(began.elapsed());
        janitor.stop();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(recovered, "the host never recovered in run {run}");
    }

    println!("janitor_recovery: interval {} s", EVERY.as_secs());
    for run in 0..3 {
        println!(
            "run {run}: down to up {:>8.2} ms, fault cleared to recovered {:>8.2} ms",
            at_spawn[run].as_secs_f64() * 1000.0,
            after_clear[run].as_secs_f64() * 1000.0
        );
    }
    let max = |samples: &[Duration]| {
        samples
            .iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .fold(0.0f64, f64::max)
    };
    println!(
        "max: down to up {:.2} ms, fault cleared to recovered {:.2} ms",
        max(&at_spawn),
        max(&after_clear)
    );
    std::io::stdout().flush().ok();
}

/// The two per-job parsers on the dispatch and relay paths. Numbers only, no
/// assertion: run it before and after a change to the code they cover.
#[test]
#[ignore = "measurement: cargo test --release --test latency -- --ignored --nocapture arg_and_stderr_cost"]
fn arg_and_stderr_cost() {
    use rrffmpeg::config::{Host, PathMap};
    use rrffmpeg::paths::to_worker;

    let host = Host {
        name: "worker".into(),
        concurrency: 1,
        capabilities: vec![],
        local: false,
        ffmpeg_bin: None,
        ffprobe_bin: None,
        agent_addr: None,
        ssh_args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        paths: vec![
            PathMap {
                from: "/home/alice".into(),
                to: "/home/bob/media".into(),
            },
            PathMap {
                from: "/dev/shm/jellyfin-transcode".into(),
                to: "/home/bob/transcode".into(),
            },
        ],
        reads: Vec::new(),
        prefer: 0,
    };
    // The shape of a real Jellyfin call: two mapped inputs, one output, and
    // the flags between them that match no rule.
    let args: Vec<OsString> = [
        "file:/home/alice/media/film.mkv",
        "-map",
        "0:V:0",
        "-c:v",
        "hevc_vaapi",
        "file:/dev/shm/jellyfin-transcode/seg%05d.ts",
    ]
    .iter()
    .map(OsString::from)
    .collect();

    const N: usize = 200_000;
    for _ in 0..1000 {
        for arg in &args {
            std::hint::black_box(to_worker(&host, arg));
        }
    }
    let began = Instant::now();
    for _ in 0..N {
        for arg in &args {
            std::hint::black_box(to_worker(&host, arg));
        }
    }
    let path_ns = began.elapsed().as_secs_f64() * 1e9 / (N * args.len()) as f64;
    println!("path rewrite          n={N}  {path_ns:8.1} ns per argument");

    // One poll of ffmpeg's stderr: the stats line, repeated, inside the 4 KiB
    // read the relay hands over.
    let line = "frame= 1491 fps=123 q=-0.0 size=  512kB time=00:01:00.39 \
                bitrate=1006.2kbits/s speed=   5x\r";
    let chunk: Vec<u8> = line.repeat(4096 / line.len()).into_bytes();
    const M: usize = 50_000;
    for _ in 0..1000 {
        rrffmpeg::progress::feed(&chunk);
    }
    rrffmpeg::progress::take();
    let began = Instant::now();
    for _ in 0..M {
        rrffmpeg::progress::feed(&chunk);
    }
    let stats = rrffmpeg::progress::take();
    let feed_ns = began.elapsed().as_secs_f64() * 1e9 / M as f64;
    let lines = (chunk.len() / line.len()) as f64;
    println!(
        "stderr stats parse    n={M}  {feed_ns:8.1} ns per {:.0} B chunk, {:.1} ns per line",
        chunk.len() as f64,
        feed_ns / lines
    );
    println!(
        "parsed fps {:?} speed {:?} src {:?}",
        stats.fps, stats.speed, stats.src_w
    );
    std::io::stdout().flush().ok();
}

/// Fixed cost every ffmpeg call pays before it dispatches: read and parse the
/// config, build the TLS client config, open the journal. The transport and the
/// process image are measured elsewhere; this isolates the setup.
#[test]
#[ignore = "measurement: cargo test --release --test latency -- --ignored --nocapture startup_cost"]
fn startup_cost() {
    const N: usize = 2000;
    let dir = std::env::temp_dir().join(format!("rrffmpeg-latency-startup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    generate_cert(&dir, &["127.0.0.1".to_string()]).expect("cert");

    let text = r#"
agent_token = "t"
agent_cert = "/tmp/cert.pem"

[[hosts]]
name = "worker"
concurrency = 4
capabilities = ["vaapi"]

[[hosts.paths]]
from = "/home/alice"
to = "/home/bob/media"
"#;
    let per = |label: &str, mut f: Box<dyn FnMut()>| {
        for _ in 0..100 {
            f();
        }
        let began = Instant::now();
        for _ in 0..N {
            f();
        }
        let us = began.elapsed().as_secs_f64() * 1e6 / N as f64;
        println!("{label:<28} {us:8.1} us per call");
        std::io::stdout().flush().ok();
    };

    per(
        "Config::from_toml",
        Box::new(|| {
            std::hint::black_box(
                rrffmpeg::config::Config::from_toml(std::hint::black_box(text)).expect("cfg"),
            );
        }),
    );
    per(
        "Agent::load (TLS config)",
        Box::new(|| {
            std::hint::black_box(Agent::load("t".into(), &dir.join("cert.pem")).expect("agent"));
        }),
    );
    per(
        "Journal::open + drop",
        Box::new(|| {
            std::hint::black_box(Journal::open(&dir).expect("journal"));
        }),
    );
    let hosts = rrffmpeg::config::Config::from_toml(text)
        .expect("cfg")
        .hosts;
    per(
        "Scheduler::new + LockDir",
        Box::new(|| {
            std::hint::black_box(Scheduler::new(
                hosts.clone(),
                Arc::new(LockDir::new(&dir)),
                Duration::from_secs(60),
            ));
        }),
    );

    let _ = std::fs::remove_dir_all(&dir);
}
