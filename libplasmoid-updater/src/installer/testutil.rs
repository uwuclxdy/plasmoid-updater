// SPDX-License-Identifier: GPL-3.0-or-later

//! Test-only capture of the log records this crate emits, so a check whose only
//! evidence is a log line can read it back together with its target.

use std::sync::Mutex;

static LOG_LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct CapturingLogger;

impl log::Log for CapturingLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if let Ok(mut lines) = LOG_LINES.lock() {
            lines.push(format!("{}: {}", record.target(), record.args()));
        }
    }

    fn flush(&self) {}
}

/// Installs the capturing logger, once per test process.
pub(crate) fn capture_logs() {
    static LOGGER: CapturingLogger = CapturingLogger;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Debug);
    });
}

/// Every line the crate has logged in this process, as `<target>: <message>`.
pub(crate) fn captured_logs() -> Vec<String> {
    LOG_LINES
        .lock()
        .map(|lines| lines.clone())
        .unwrap_or_default()
}
