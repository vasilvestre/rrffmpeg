//! Print what the journal says about dispatch cost and job duration.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match rrffmpeg::stats::main(&args) {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("rrffmpeg-stats: {e}");
            std::process::ExitCode::from(1)
        }
    }
}
