//! The worker side of the agent transport: one daemon replaces one ssh per job.
//!
//! ```text
//! rrffmpeg-agent --init /etc/rrffmpeg --san worker --san 10.8.0.2
//! RRFFMPEG_AGENT_TOKEN=SECRET rrffmpeg-agent \
//!     --listen 10.8.0.2:9876 \
//!     --cert /etc/rrffmpeg/cert.pem --key /etc/rrffmpeg/key.pem
//! ```
//!
//! Prefer `RRFFMPEG_AGENT_TOKEN` over `--token`: a command line is visible in
//! `ps` and in the shell history.

use std::path::PathBuf;

fn main() {
    let mut listen = String::from("127.0.0.1:9876");
    let mut token = std::env::var("RRFFMPEG_AGENT_TOKEN").unwrap_or_default();
    let mut cert: Option<PathBuf> = None;
    let mut key: Option<PathBuf> = None;
    let mut init: Option<PathBuf> = None;
    let mut sans: Vec<String> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_default();
        match arg.as_str() {
            "--listen" => listen = value(),
            "--token" => token = value(),
            "--cert" => cert = Some(value().into()),
            "--key" => key = Some(value().into()),
            "--init" => init = Some(value().into()),
            "--san" => sans.push(value()),
            "-h" | "--help" => {
                println!(
                    "usage: rrffmpeg-agent --cert FILE --key FILE [--listen ADDR] [--token SECRET]\n       rrffmpeg-agent --init DIR [--san NAME]..."
                );
                return;
            }
            "--version" => {
                println!("rrffmpeg-agent {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => {
                eprintln!("rrffmpeg-agent: unknown option {other}");
                std::process::exit(2);
            }
        }
    }

    if let Some(dir) = init {
        match rrffmpeg::agent::generate_cert(&dir, &sans) {
            Ok(()) => {
                println!(
                    "wrote {} and {}",
                    dir.join("cert.pem").display(),
                    dir.join("key.pem").display()
                );
                return;
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    if token.is_empty() {
        eprintln!("rrffmpeg-agent: a token is required, pass --token or RRFFMPEG_AGENT_TOKEN");
        std::process::exit(2);
    }
    let (Some(cert), Some(key)) = (cert, key) else {
        eprintln!("rrffmpeg-agent: --cert and --key are required, run --init DIR first");
        std::process::exit(2);
    };
    if let Err(e) = rrffmpeg::agent::serve(&listen, token, &cert, &key) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
