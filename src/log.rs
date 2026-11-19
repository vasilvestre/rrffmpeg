//! Log levels for the dispatcher.
//!
//! `RRFFMPEG_LOG` sets the threshold: `off`, `error`, `warn` or `debug`. The default is `warn`, so a production run prints what it printed
//! before this module existed. Use `debug` to see the byte count each job
//! relays on stdout and stderr, which is how you measure the cost of a large
//! passthrough such as `ffmpeg -encoders`.
//!
//! Everything goes to stderr. stdout belongs to ffmpeg.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Off,
    Error,
    Warn,
    Debug,
}

impl Level {
    pub fn parse(text: &str) -> Option<Level> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Level::Off),
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "debug" | "trace" => Some(Level::Debug),
            _ => None,
        }
    }
}

/// Read once: the level does not change during a run, and a per-line
/// `std::env::var` would cost more than the line it guards.
pub fn level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("RRFFMPEG_LOG")
            .ok()
            .and_then(|text| Level::parse(&text))
            .unwrap_or(Level::Warn)
    })
}

pub fn enabled(level: Level) -> bool {
    level != Level::Off && level <= self::level()
}

/// `log_at!(Level::Debug, "job {} relayed {} B", id, bytes);`
#[macro_export]
macro_rules! log_at {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            eprintln!("rrffmpeg: {}", format_args!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_parse_and_rank() {
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse(" warning "), Some(Level::Warn));
        assert_eq!(Level::parse("nope"), None);
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Debug);
    }
}
