// SPDX-License-Identifier: GPL-3.0-or-later

//! Test-only capture of the log records this crate emits, so a check whose only
//! evidence is a log line can read it back together with its target.

use std::sync::Mutex;

/// One captured record, keeping the level and target the test filters on.
struct Captured {
    level: log::Level,
    target: String,
    message: String,
}

impl Captured {
    fn line(&self) -> String {
        format!("{}: {}", self.target, self.message)
    }
}

static LOG_LINES: Mutex<Vec<Captured>> = Mutex::new(Vec::new());

struct CapturingLogger;

impl log::Log for CapturingLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if let Ok(mut lines) = LOG_LINES.lock() {
            lines.push(Captured {
                level: record.level(),
                target: record.target().to_string(),
                message: record.args().to_string(),
            });
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
        .map(|lines| lines.iter().map(Captured::line).collect())
        .unwrap_or_default()
}

/// The messages `target` logged at `Warn` or above, in order.
pub(crate) fn captured_warnings_for(target: &str) -> Vec<String> {
    LOG_LINES
        .lock()
        .map(|lines| {
            lines
                .iter()
                .filter(|line| line.level <= log::Level::Warn && line.target.as_str() == target)
                .map(|line| line.message.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Runs `run`, returning the lines logged from the moment it started.
///
/// The buffer is shared by every test in the process and never cleared, so a
/// line another test's thread logs in the window can appear here too; every
/// caller checks with `any`, and clearing would make a concurrent test's
/// evidence vanish.
pub(crate) fn captured_logs_while(run: impl FnOnce()) -> Vec<String> {
    capture_logs();
    let mark = LOG_LINES.lock().map(|lines| lines.len()).unwrap_or(0);
    run();
    captured_logs().into_iter().skip(mark).collect()
}
