// SPDX-License-Identifier: GPL-3.0-or-later

//! The stderr log backend: `-q` keeps errors, the default keeps warnings,
//! `-v` adds info and `-vv` debug. `RUST_LOG` is deliberately not consulted.

use std::io::{IsTerminal as _, Write as _};
use std::sync::OnceLock;

use log::{Level, LevelFilter, Log, Metadata, Record};

static LOGGER: OnceLock<StderrLogger> = OnceLock::new();

/// Installs the logger, once per process.
pub fn init(quiet: bool, verbose: u8) {
    let logger = LOGGER.get_or_init(|| StderrLogger {
        max_level: level_filter(quiet, verbose),
        color: color_enabled(),
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(logger.max_level);
    }
}

fn level_filter(quiet: bool, verbose: u8) -> LevelFilter {
    if quiet {
        return LevelFilter::Error;
    }
    match verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        _ => LevelFilter::Debug,
    }
}

/// Colour needs a terminal that has not opted out.
fn color_enabled() -> bool {
    std::io::stderr().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var("TERM")
            .map(|term| term != "dumb")
            .unwrap_or(true)
}

struct StderrLogger {
    max_level: LevelFilter,
    color: bool,
}

impl Log for StderrLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.max_level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(
            stderr,
            "{}",
            line(record.level(), &record.args().to_string(), self.color)
        );
        let _ = stderr.flush();
    }

    fn flush(&self) {}
}

/// One log line: `<level>: <message>`, the level lowercase and bold only on a
/// terminal that takes colour.
///
/// A message's own newlines keep their lines under the single entry, indented,
/// so a multi-line message never reads as several log entries.
fn line(level: Level, message: &str, color: bool) -> String {
    let level = level.as_str().to_ascii_lowercase();
    let message = message.replace('\n', "\n  ");
    if color {
        format!("\u{1b}[1m{level}:\u{1b}[0m {message}")
    } else {
        format!("{level}: {message}")
    }
}

#[cfg(test)]
mod tests {
    use super::{level_filter, line};
    use log::LevelFilter;

    #[test]
    fn a_line_is_the_lowercase_level_then_the_message() {
        assert_eq!(
            line(log::Level::Warn, "watch out", false),
            "warn: watch out"
        );
        assert_eq!(line(log::Level::Error, "boom", false), "error: boom");
        assert_eq!(line(log::Level::Debug, "step", false), "debug: step");
    }

    #[test]
    fn only_the_level_prefix_carries_colour() {
        assert_eq!(
            line(log::Level::Warn, "watch out", true),
            "\u{1b}[1mwarn:\u{1b}[0m watch out"
        );
    }

    #[test]
    fn a_multi_line_message_keeps_one_entry() {
        assert_eq!(
            line(log::Level::Error, "unclosed group\nin pattern", false),
            "error: unclosed group\n  in pattern"
        );
        assert_eq!(
            line(log::Level::Error, "unclosed group\nin pattern", true),
            "\u{1b}[1merror:\u{1b}[0m unclosed group\n  in pattern"
        );
    }

    #[test]
    fn the_flags_pick_the_level() {
        assert_eq!(level_filter(true, 0), LevelFilter::Error);
        assert_eq!(level_filter(false, 0), LevelFilter::Warn);
        assert_eq!(level_filter(false, 1), LevelFilter::Info);
        assert_eq!(level_filter(false, 2), LevelFilter::Debug);
    }
}
