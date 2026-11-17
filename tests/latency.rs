//! Latency measurements for the dispatcher, on loopback.
//!
//! These are measurements, not assertions: they print numbers and never fail.
//! Run them by hand, and keep the machine and the load in the report.
//!
//! ```text
//! cargo test --release --test latency -- --ignored --nocapture
//! ```

use rrffmpeg::agent::{Agent, generate_cert, serve_on, server_config};
use rrffmpeg::transport::Transport;
use rrffmpeg::{Job, Program};
use std::ffi::OsString;
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
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
        // /bin/true keeps the measurement on the dispatcher: no codec work.
        ffmpeg_bin: Some("/bin/true".into()),
        ffprobe_bin: Some("/bin/true".into()),
        agent_addr: Some(addr),
        paths: vec![],
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
        capability: None,
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
