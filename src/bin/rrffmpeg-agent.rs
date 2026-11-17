//! The worker side of the agent transport: one daemon replaces one ssh per job.
//!
//! `rrffmpeg-agent --listen 0.0.0.0:9876 --token SECRET`
//!
//! Prefer `RRFFMPEG_AGENT_TOKEN` over `--token`: a command line is visible in
//! `ps` and in the shell history.

fn main() {
    let mut listen = String::from("127.0.0.1:9876");
    let mut token = std::env::var("RRFFMPEG_AGENT_TOKEN").unwrap_or_default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().unwrap_or_default(),
            "--token" => token = args.next().unwrap_or_default(),
            "-h" | "--help" => {
                println!("usage: rrffmpeg-agent [--listen ADDR] [--token SECRET]");
                return;
            }
            other => {
                eprintln!("rrffmpeg-agent: unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    if token.is_empty() {
        eprintln!("rrffmpeg-agent: a token is required, pass --token or RRFFMPEG_AGENT_TOKEN");
        std::process::exit(2);
    }
    if let Err(e) = rrffmpeg::agent::serve(&listen, token) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
