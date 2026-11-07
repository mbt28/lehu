//! Logging: the `log` facade, written to stderr one line at a time with the
//! seconds since start, the level and the module. The level comes from
//! `system.log-level`; `verbose` and `protocol` both map to trace, so every
//! crate uses the five standard macros.

use std::io::Write as _;

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::clock;
use crate::settings::LogLevel;

struct Stderr;

impl Log for Stderr {
    /// The level applies to LEHU's own crates. A dependency's debug lines
    /// (a USB library's per-transfer trace) would otherwise flood the log
    /// at `debug`, and writing them to the card stalls the threads that
    /// log; they come through only at `verbose` and above, or when they
    /// are warnings.
    fn enabled(&self, metadata: &Metadata) -> bool {
        let max = log::max_level();
        if metadata.level() > max {
            return false;
        }
        metadata.level() <= Level::Warn
            || max >= LevelFilter::Trace
            || metadata.target().starts_with("lehu")
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ms = clock::now_ms();
        let level = match record.level() {
            Level::Error => "error",
            Level::Warn => "warn ",
            Level::Info => "info ",
            Level::Debug => "debug",
            Level::Trace => "trace",
        };
        // One write per line, so lines from several threads do not interleave.
        let line = format!(
            "[{:5}.{:03}] {level} {}: {}\n",
            ms / 1000,
            ms % 1000,
            record.target(),
            record.args()
        );
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }

    fn flush(&self) {}
}

static LOGGER: Stderr = Stderr;

pub fn filter_of(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::Error,
        LogLevel::Warn => LevelFilter::Warn,
        LogLevel::Info => LevelFilter::Info,
        LogLevel::Debug => LevelFilter::Debug,
        LogLevel::Verbose | LogLevel::Protocol => LevelFilter::Trace,
    }
}

/// Install the logger at `level`. Calling it again only changes the level.
pub fn init(level: LogLevel) {
    clock::start();
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(filter_of(level));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependencies_are_quiet_below_verbose() {
        let meta = |target: &'static str, level: Level| {
            Metadata::builder().target(target).level(level).build()
        };
        log::set_max_level(LevelFilter::Debug);
        assert!(Stderr.enabled(&meta("lehu_aa::machine", Level::Debug)));
        assert!(!Stderr.enabled(&meta("nusb::platform", Level::Debug)));
        assert!(Stderr.enabled(&meta("nusb::platform", Level::Warn)));
        log::set_max_level(LevelFilter::Trace);
        assert!(Stderr.enabled(&meta("nusb::platform", Level::Debug)));
        log::set_max_level(LevelFilter::Info);
        assert!(!Stderr.enabled(&meta("lehu_aa::machine", Level::Debug)));
    }

    #[test]
    fn levels_map() {
        assert_eq!(filter_of(LogLevel::Error), LevelFilter::Error);
        assert_eq!(filter_of(LogLevel::Info), LevelFilter::Info);
        assert_eq!(filter_of(LogLevel::Protocol), LevelFilter::Trace);
    }

    #[test]
    fn init_sets_the_level_and_logs() {
        init(LogLevel::Debug);
        assert_eq!(log::max_level(), LevelFilter::Debug);
        log::debug!("a line from the test");
        init(LogLevel::Warn);
        assert_eq!(log::max_level(), LevelFilter::Warn);
    }
}
