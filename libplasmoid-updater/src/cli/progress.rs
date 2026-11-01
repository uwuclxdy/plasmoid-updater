// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant};

use parking_lot::Mutex as ParkingMutex;

use crate::{Event, Reporter};

const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Animated redraws happen at most once per this interval; chunk updates in
/// between only update the state and are caught up on the next render.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// A [`Reporter`] drawing download and install progress on stderr.
///
/// Coloured and animated on a TTY stderr; an uncoloured, non-animated
/// milestone-per-line fallback when `NO_COLOR` is set non-empty, `TERM=dumb`,
/// or stderr is not a TTY.
pub struct ProgressReporter {
    state: ParkingMutex<State>,
    animated: bool,
}

struct State {
    rows: Vec<Row>,
    /// Log lines not yet written to the terminal (animated mode only).
    logs: Vec<String>,
    /// Row lines currently rendered at the bottom of stderr (animated mode).
    drawn: usize,
    last_redraw: Option<Instant>,
}

struct Row {
    /// The component's installed path: the key a row is looked up by.
    id: String,
    /// The component's display name: what the row renders.
    name: String,
    bytes: u64,
    total: Option<u64>,
    done: Option<bool>,
}

impl ProgressReporter {
    /// Creates a reporter writing to stderr.
    pub fn new() -> Self {
        let animated = std::io::stderr().is_terminal()
            && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        Self {
            state: ParkingMutex::new(State {
                rows: Vec::new(),
                logs: Vec::new(),
                drawn: 0,
                last_redraw: None,
            }),
            animated,
        }
    }

    fn print_line(&self, line: &str) {
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
        let _ = err.flush();
    }

    /// Renders pending log lines and the row block in animated mode.
    ///
    /// The row block is erased and reprinted below any new log lines, so log
    /// lines and rows never overwrite each other: logs accumulate above the
    /// block and scroll away, the block stays pinned to the bottom.
    fn render(&self, state: &mut State) {
        let mut err = std::io::stderr().lock();
        if state.drawn > 0 {
            let _ = write!(err, "\x1b[{}A", state.drawn);
            let _ = write!(err, "\x1b[J");
        }
        for line in state.logs.drain(..) {
            let _ = writeln!(err, "{line}");
        }
        for row in &state.rows {
            let _ = write!(err, "\r\x1b[K{}\n", render_row(row));
        }
        state.drawn = state.rows.len();
        state.last_redraw = Some(Instant::now());
        let _ = err.flush();
    }
}

impl Default for ProgressReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ProgressReporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressReporter")
            .field("animated", &self.animated)
            .finish_non_exhaustive()
    }
}

impl Reporter for ProgressReporter {
    fn event(&self, event: &Event) {
        match event {
            Event::Checking => {
                let mut state = self.state.lock();
                if self.animated {
                    state.logs.push("checking for updates...".to_string());
                    self.render(&mut state);
                } else {
                    drop(state);
                    self.print_line("checking for updates...");
                }
            }
            Event::Downloading {
                name,
                id,
                bytes,
                total,
            } => {
                let mut state = self.state.lock();
                let is_new = match state.rows.iter_mut().find(|r| r.id == *id) {
                    Some(row) => {
                        row.bytes = *bytes;
                        row.total = row.total.or(*total);
                        false
                    }
                    None => {
                        state.rows.push(Row {
                            id: id.clone(),
                            name: name.clone(),
                            bytes: *bytes,
                            total: *total,
                            done: None,
                        });
                        true
                    }
                };
                if self.animated {
                    // New rows and log lines render at once; per-chunk byte
                    // updates are throttled (state stays current either way).
                    let due = is_new
                        || !state
                            .last_redraw
                            .is_some_and(|t| t.elapsed() < REDRAW_INTERVAL);
                    if due {
                        self.render(&mut state);
                    }
                } else if is_new {
                    drop(state);
                    self.print_line(&format!("downloading {name}..."));
                }
            }
            Event::Installed { name, id } => self.finish_row(id, name),
            Event::Failed { name, id, message } => {
                let mut state = self.state.lock();
                if let Some(row) = state.rows.iter_mut().find(|r| r.id == *id) {
                    row.done = Some(false);
                }
                if self.animated {
                    state
                        .logs
                        .push(format!("{RED}failed:{RESET} {name}: {message}"));
                    self.render(&mut state);
                } else {
                    drop(state);
                    self.print_line(&format!("failed {name}: {message}"));
                }
            }
        }
    }
}

impl ProgressReporter {
    fn finish_row(&self, id: &str, name: &str) {
        let mut state = self.state.lock();
        if let Some(row) = state.rows.iter_mut().find(|r| r.id == id) {
            row.done = Some(true);
        } else {
            state.rows.push(Row {
                id: id.to_string(),
                name: name.to_string(),
                bytes: 0,
                total: None,
                done: Some(true),
            });
        }
        if self.animated {
            self.render(&mut state);
        } else {
            drop(state);
            self.print_line(&format!("installed {name}"));
        }
    }
}

fn render_row(row: &Row) -> String {
    let total = row.total.map(fmt_bytes).unwrap_or_else(|| "?".into());
    let pct = row
        .total
        .filter(|t| *t > 0)
        .map(|t| format!(" ({:>3}%)", row.bytes * 100 / t))
        .unwrap_or_default();
    match row.done {
        Some(true) => format!("{GREEN}\u{2713}{RESET} {}{CYAN}{pct}{RESET}", row.name),
        Some(false) => format!("{RED}\u{2717}{RESET} {}", row.name),
        None => format!(
            "{YELLOW}\u{2937}{RESET} {} {CYAN}{}/{total}{RESET}{pct}",
            row.name, row.bytes
        ),
    }
}

fn fmt_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A progress row must be found by the event's `id`, so a component's
    /// bytes and outcome land on its own row — two components sharing a
    /// display name keep two rows — and it must render the display `name`
    /// rather than the technical `id`.
    #[test]
    fn rows_are_keyed_by_id_and_render_the_display_name() {
        let reporter = ProgressReporter::new();
        reporter.event(&Event::Downloading {
            name: "Fake Scheme".to_string(),
            id: "FakeScheme.colors".to_string(),
            bytes: 10,
            total: Some(100),
        });
        reporter.event(&Event::Downloading {
            name: "Fake Scheme".to_string(),
            id: "FakeScheme.colors".to_string(),
            bytes: 50,
            total: Some(100),
        });
        reporter.event(&Event::Installed {
            name: "Fake Scheme".to_string(),
            id: "FakeScheme.colors".to_string(),
        });
        reporter.event(&Event::Downloading {
            name: "Twin".to_string(),
            id: "first".to_string(),
            bytes: 1,
            total: None,
        });
        reporter.event(&Event::Downloading {
            name: "Twin".to_string(),
            id: "second".to_string(),
            bytes: 1,
            total: None,
        });

        let state = reporter.state.lock();
        assert_eq!(state.rows.len(), 3);
        assert_eq!(state.rows[0].id, "FakeScheme.colors");
        assert_eq!(state.rows[0].name, "Fake Scheme");
        assert_eq!(state.rows[0].bytes, 50);
        assert_eq!(state.rows[0].done, Some(true));
        assert_eq!(state.rows[1].id, "first");
        assert_eq!(state.rows[2].id, "second");
    }
}
