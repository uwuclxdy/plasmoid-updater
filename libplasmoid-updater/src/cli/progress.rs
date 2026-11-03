// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex as ParkingMutex;

use super::output::cells;
use super::theme::{self, Token};
use super::{Stream, styled};
use crate::{Event, Reporter};

/// Animated redraws happen at most once per this interval; chunk updates in
/// between only update the state and are caught up on the next render.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// The spinner advances one frame per this interval while it is drawn.
const FRAME_INTERVAL: Duration = Duration::from_millis(80);

/// How many cells a download row's progress bar spans.
const BAR_WIDTH: u64 = 12;

/// The width a redrawn line is clipped to when the terminal reports none: a
/// pty without a window size cannot wrap where the bytes say it does, and 80
/// is the conventional floor.
pub(super) const DEFAULT_WIDTH: usize = 80;

/// Every live reporter, so [`print_above`] can find the one drawing at the
/// bottom of stderr. At most one is live: a new reporter settles the previous
/// one first. A weak entry dies with its reporter.
static LIVE: ParkingMutex<Vec<Weak<Inner>>> = ParkingMutex::new(Vec::new());

/// A [`Reporter`] drawing download and install progress on stderr.
///
/// Colour follows [`styled`] for stderr; animation (redraws that move the
/// cursor) additionally needs stderr to be a terminal, so a forced colour into
/// a pipe colours the finished rows without animating them.
///
/// One reporter is live at a time: creating one settles the reporter already
/// drawing, so two blocks never fight over the same rows. The reporter
/// registers itself as that live block, so [`print_above`] writes log lines
/// above its rows and redraws them intact. It settles itself once every row
/// has an outcome; dropping it does the same. An event after it settled opens
/// a fresh block below whatever was printed since.
///
/// On a terminal every redrawn line is clipped to the terminal's width and any
/// control character in a name or message is flattened, so a line the reporter
/// owns always takes exactly one terminal row.
pub struct ProgressReporter {
    inner: Arc<Inner>,
}

struct Inner {
    state: ParkingMutex<State>,
    colored: bool,
    animated: bool,
    /// Set while a frame-ticking thread runs, so at most one exists.
    spinning: AtomicBool,
    /// Where `render` and `print_line` write: stderr in production, a buffer in
    /// unit tests so an animated reporter never moves the test runner's own
    /// cursor.
    out: ParkingMutex<Box<dyn Write + Send>>,
}

#[derive(Default)]
struct State {
    rows: Vec<Row>,
    /// Lines not yet written to the terminal (animated mode only): log lines
    /// from `print_above` and the finished rows a finished component leaves.
    logs: Vec<String>,
    /// Lines the row block currently occupies at the bottom of stderr.
    drawn: usize,
    /// The width the previous frame was drawn at.
    last_width: Option<usize>,
    last_redraw: Option<Instant>,
    /// The component count while the checking line is live.
    checking: Option<usize>,
    frame: usize,
    /// The display has been settled; nothing may redraw over what follows.
    finished: bool,
}

/// One in-progress component: the live block holds only these. A finished
/// component leaves the block and its row is written once above it.
struct Row {
    /// The component's installed path: the key a row is looked up by.
    id: String,
    /// The component's display name: what the row renders.
    name: String,
    bytes: u64,
    total: Option<u64>,
}

/// One run of a redrawn line: `text` is coloured by `token` (bold when the
/// flag says so) only while styling is on.
struct Piece {
    token: Option<Token>,
    bold: bool,
    text: String,
}

impl Piece {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            token: None,
            bold: false,
            text: text.into(),
        }
    }

    fn tinted(token: Token, text: impl Into<String>) -> Self {
        Self {
            token: Some(token),
            bold: false,
            text: text.into(),
        }
    }

    fn emphasized(token: Token, text: impl Into<String>) -> Self {
        Self {
            token: Some(token),
            bold: true,
            text: text.into(),
        }
    }

    fn cells(&self) -> usize {
        cells(&self.text)
    }
}

impl ProgressReporter {
    /// Creates a reporter writing to stderr, settling any reporter already
    /// drawing.
    pub fn new() -> Self {
        let colored = styled(Stream::Stderr);
        let animated = colored && std::io::stderr().is_terminal();
        let inner = Arc::new(Inner {
            state: ParkingMutex::new(State::default()),
            colored,
            animated,
            spinning: AtomicBool::new(false),
            out: ParkingMutex::new(Box::new(std::io::stderr())),
        });
        let mut live = LIVE.lock();
        for previous in live.iter().filter_map(Weak::upgrade) {
            previous.settle();
        }
        live.clear();
        live.push(Arc::downgrade(&inner));
        drop(live);
        Self { inner }
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
            .field("colored", &self.inner.colored)
            .field("animated", &self.inner.animated)
            .finish_non_exhaustive()
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        let mine = Arc::downgrade(&self.inner);
        LIVE.lock().retain(|weak| !weak.ptr_eq(&mine));
        self.inner.settle();
    }
}

impl Reporter for ProgressReporter {
    fn event(&self, event: &Event) {
        match event {
            Event::Checking { components } => self.checking(*components),
            Event::Checked => self.checked(),
            Event::Downloading {
                name,
                id,
                bytes,
                total,
            } => self.downloading(name, id, *bytes, *total),
            Event::Installed { name, id, version } => self.installed(name, id, version),
            Event::Failed { name, id, message } => self.failed(name, id, message),
        }
    }
}

impl ProgressReporter {
    fn checking(&self, components: usize) {
        if !self.inner.animated {
            self.inner.print_line(&checking_text(components));
            return;
        }
        {
            let mut state = self.inner.state.lock();
            self.inner.reopen(&mut state);
            state.checking = Some(components);
            state.frame = 0;
            self.inner.render(&mut state);
        }
        start_spinner(&self.inner);
    }

    fn checked(&self) {
        if !self.inner.animated {
            return;
        }
        let mut state = self.inner.state.lock();
        if state.checking.take().is_none() {
            return;
        }
        self.inner.render(&mut state);
        self.inner.settle_if_idle(&mut state);
    }

    fn downloading(&self, name: &str, id: &str, bytes: u64, total: Option<u64>) {
        let mut state = self.inner.state.lock();
        self.inner.reopen(&mut state);
        state.checking = None;
        let is_new = match state.rows.iter_mut().find(|row| row.id == id) {
            Some(row) => {
                row.bytes = bytes;
                row.total = row.total.or(total);
                false
            }
            None => {
                state.rows.push(Row {
                    id: id.to_string(),
                    name: flatten(name),
                    bytes,
                    total,
                });
                true
            }
        };
        if !self.inner.animated {
            drop(state);
            if is_new {
                self.inner
                    .print_line(&format!("downloading {}...", flatten(name)));
            }
            return;
        }
        // New rows render at once; per-chunk byte updates are throttled (the
        // state stays current either way).
        let due = is_new
            || !state
                .last_redraw
                .is_some_and(|at| at.elapsed() < REDRAW_INTERVAL);
        if due {
            self.inner.render(&mut state);
            self.inner.settle_if_idle(&mut state);
        }
        drop(state);
        start_spinner(&self.inner);
    }

    fn installed(&self, name: &str, id: &str, version: &str) {
        let version = flatten(version);
        self.finish_row(id, done_pieces(name, Some(&version)));
    }

    fn failed(&self, name: &str, id: &str, message: &str) {
        let message = flatten(message);
        self.finish_row(id, failed_pieces(name, &message));
    }

    /// Removes the component's in-progress row and writes its finished line once
    /// above the block, in completion order, never to be redrawn. On a terminal
    /// the line is clipped to the width so it takes exactly one row; piped
    /// output keeps the full line.
    fn finish_row(&self, id: &str, pieces: Vec<Piece>) {
        let mut state = self.inner.state.lock();
        self.inner.reopen(&mut state);
        state.checking = None;
        state.rows.retain(|row| row.id != id);
        let line = if self.inner.animated {
            let width = terminal_width().unwrap_or(DEFAULT_WIDTH);
            self.inner.line_text(&pieces, width)
        } else {
            self.inner.line_text(&pieces, usize::MAX)
        };
        if self.inner.animated {
            state.logs.push(line);
            self.inner.render(&mut state);
            self.inner.settle_if_idle(&mut state);
        } else {
            drop(state);
            self.inner.print_line(&line);
        }
    }
}

impl Inner {
    fn print_line(&self, line: &str) {
        let mut out = self.out.lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    /// Ends the checking line and marks the block settled, so a line printed
    /// after it is never redrawn over. Settling again does nothing.
    fn settle(&self) {
        let mut state = self.state.lock();
        if state.finished {
            return;
        }
        state.finished = true;
        let had_checking = state.checking.take().is_some();
        if self.animated && had_checking {
            self.render(&mut state);
        }
    }

    /// Settles a block with nothing left to animate.
    fn settle_if_idle(&self, state: &mut State) {
        state.finished = !spinning(state);
    }

    /// Opens a fresh block below whatever a settled reporter left on screen.
    fn reopen(&self, state: &mut State) {
        if !state.finished {
            return;
        }
        state.finished = false;
        state.drawn = 0;
        state.last_width = None;
        state.rows.clear();
        state.logs.clear();
        state.checking = None;
        state.frame = 0;
    }

    /// Renders pending log lines and the row block in animated mode.
    ///
    /// The row block is erased and reprinted below any new log lines, so log
    /// lines and rows never overwrite each other: logs and finished rows
    /// accumulate above the block and scroll away, the block stays pinned to
    /// the bottom. The block holds only rows still in progress, bounded by the
    /// terminal height so a block taller than the screen never pushes stale
    /// frames into scrollback.
    fn render(&self, state: &mut State) {
        let (width, height) = terminal_size();
        let erase = erase_rows(state, width);
        // The whole frame goes out in one write, so a line another thread
        // writes to stderr directly lands before or after it, never inside.
        let mut frame: Vec<u8> = Vec::new();
        let out = &mut frame;
        if erase > 0 {
            let _ = write!(out, "\x1b[{erase}A");
            // Clear each row the previous frame occupies, one EL per row, never
            // ED: a terminal that treats ED from the home row as a screen clear
            // (tmux with scroll-on-clear) copies the visible screen into
            // history on every redraw, one spinner frame per tick. EL never
            // scrolls.
            for _ in 0..erase {
                let _ = write!(out, "\r\x1b[2K\x1b[B");
            }
            let _ = write!(out, "\x1b[{erase}A");
        }
        for line in state.logs.drain(..) {
            let _ = writeln!(out, "{line}");
        }
        let mut lines: Vec<String> = Vec::new();
        if let Some(components) = state.checking {
            let pieces = checking_pieces(components, state.frame);
            lines.push(self.line_text(&pieces, width));
        }
        for row in &state.rows {
            let pieces = download_pieces(row, state.frame);
            lines.push(self.line_text(&pieces, width));
        }
        if let Some(rows) = height {
            let capacity = rows.saturating_sub(1);
            if lines.len() > capacity {
                if capacity == 0 {
                    lines.clear();
                } else {
                    let hidden = rows_hidden(lines.len(), capacity);
                    lines.truncate(capacity - 1);
                    let more = Piece::tinted(Token::TextDim, format!("+{hidden} more"));
                    lines.push(self.line_text(&[more], width));
                }
            }
        }
        for text in &lines {
            let _ = writeln!(out, "\r\x1b[K{text}");
        }
        state.drawn = lines.len();
        state.last_width = Some(width);
        state.last_redraw = Some(Instant::now());
        let mut sink = self.out.lock();
        let _ = sink.write_all(&frame);
        let _ = sink.flush();
    }

    /// Joins one line's pieces, clipping it to `width` cells with a trailing
    /// `…` when it must not wrap.
    ///
    /// A clipped line ends on a reset and every piece carries its own, so a
    /// cut can never spill a colour into the next terminal row.
    fn line_text(&self, pieces: &[Piece], width: usize) -> String {
        let total: usize = pieces.iter().map(Piece::cells).sum();
        let mut text = String::new();
        if total <= width {
            for piece in pieces {
                text.push_str(&self.paint(piece));
            }
            return text;
        }
        let mut left = width.saturating_sub(1);
        'pieces: for piece in pieces {
            let mut used = 0;
            let mut kept = String::new();
            for ch in piece.text.chars() {
                kept.push(ch);
                // Measure with the same string-width function the total used,
                // so an emoji presentation sequence (base + variation
                // selector) counts as two cells, never one.
                let used_now = cells(&kept);
                if used_now > left {
                    kept.pop();
                    text.push_str(&self.paint(&Piece {
                        token: piece.token,
                        bold: piece.bold,
                        text: kept,
                    }));
                    break 'pieces;
                }
                used = used_now;
            }
            text.push_str(&self.paint(&Piece {
                token: piece.token,
                bold: piece.bold,
                text: kept,
            }));
            left -= used;
        }
        text.push_str(&self.paint(&Piece::tinted(Token::TextDim, "\u{2026}")));
        text
    }

    fn paint(&self, piece: &Piece) -> String {
        if !self.colored {
            return piece.text.clone();
        }
        match (piece.token, piece.bold) {
            (Some(token), true) => theme::paint_bold(token, &piece.text),
            (Some(token), false) => theme::paint(token, &piece.text),
            (None, _) => piece.text.clone(),
        }
    }

    /// Writes a log line above the live row block, or plainly when this
    /// reporter has no animated block to protect.
    fn above(&self, line: &str) {
        let mut state = self.state.lock();
        if self.animated && !state.finished {
            state.logs.push(line.to_string());
            self.render(&mut state);
        } else {
            drop(state);
            self.print_line(line);
        }
    }
}

/// Writes `line` above the live [`ProgressReporter`]'s rows, so a log line
/// never erases a row and a redraw never erases a log line.
///
/// The line goes above the reporter that is live, and the reporter redraws
/// its rows below it. With no reporter live the line is written plainly on
/// stderr. Safe to call from any thread, which matters because installs run on
/// a thread pool.
///
/// # Examples
///
/// ```no_run
/// use libplasmoid_updater::cli;
///
/// cli::print_above("warn: the store advertises no checksum");
/// ```
pub fn print_above(line: &str) {
    let inner = {
        let mut live = LIVE.lock();
        live.retain(|weak| weak.strong_count() > 0);
        live.iter().find_map(Weak::upgrade)
    };
    match inner {
        Some(inner) => inner.above(line),
        None => {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{line}");
            let _ = err.flush();
        }
    }
}

/// Settles the live reporter, if any, before a caller draws its own output.
pub(super) fn settle_live() {
    let reporters: Vec<Arc<Inner>> = {
        let mut live = LIVE.lock();
        live.retain(|weak| weak.strong_count() > 0);
        live.iter().filter_map(Weak::upgrade).collect()
    };
    for inner in reporters {
        inner.settle();
    }
}

/// The stderr terminal's width in cells, when it reports one.
pub(super) fn terminal_width() -> Option<usize> {
    let size = rustix::termios::tcgetwinsize(std::io::stderr()).ok()?;
    (size.ws_col > 0).then(|| usize::from(size.ws_col))
}

/// The stderr terminal's width (with the documented fallback) and its height,
/// `None` when the terminal reports no height to bound the block by.
fn terminal_size() -> (usize, Option<usize>) {
    match rustix::termios::tcgetwinsize(std::io::stderr()) {
        Ok(size) if size.ws_col > 0 => (
            usize::from(size.ws_col),
            (size.ws_row > 0).then(|| usize::from(size.ws_row)),
        ),
        _ => (DEFAULT_WIDTH, None),
    }
}

/// Ticks the spinner while the reporter has work to show. The thread holds
/// only a weak reference, so a dropped reporter ends it at its next tick.
fn start_spinner(inner: &Arc<Inner>) {
    if inner
        .spinning
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let weak = Arc::downgrade(inner);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(FRAME_INTERVAL);
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let mut state = inner.state.lock();
            if state.finished || !spinning(&state) {
                inner.spinning.store(false, Ordering::Release);
                return;
            }
            state.frame = (state.frame + 1) % theme::SPINNER_FRAMES.len();
            inner.render(&mut state);
        }
    });
}

fn spinning(state: &State) -> bool {
    state.checking.is_some() || !state.rows.is_empty()
}

/// Replaces every control character, a newline included, with a space: a name
/// or message from the store must never move the cursor inside a redrawn line.
fn flatten(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

fn spinner(frame: usize) -> char {
    theme::SPINNER_FRAMES
        .get(frame)
        .copied()
        .unwrap_or(theme::SPINNER_FRAMES[0])
}

fn checking_text(components: usize) -> String {
    format!(
        "checking {components} component{}",
        if components == 1 { "" } else { "s" }
    )
}

fn checking_pieces(components: usize, frame: usize) -> Vec<Piece> {
    vec![
        Piece::tinted(Token::Accent, spinner(frame).to_string()),
        Piece::plain(" "),
        Piece::tinted(Token::TextDim, checking_text(components)),
    ]
}

/// Rows a block with `total` in-progress rows hides when it fits at most
/// `capacity` rows and one row holds the `+N more` summary: the summary takes
/// one of the rows the block fits, so `capacity - 1` rows stay visible.
fn rows_hidden(total: usize, capacity: usize) -> usize {
    total.saturating_sub(capacity.saturating_sub(1))
}

/// How many terminal rows the previous frame occupies at `width`. Unchanged
/// width erases exactly the block's own rows; a width change erases nothing, so
/// the previous frame is left where it is and the next block starts below it —
/// finished rows and log lines above are never erased, whatever the terminal
/// does with the old frame on a resize.
fn erase_rows(state: &State, width: usize) -> usize {
    match state.last_width {
        Some(previous) if previous != width => 0,
        _ => state.drawn,
    }
}

/// `✓ <name> <version>`, the version dim.
fn done_pieces(name: &str, version: Option<&str>) -> Vec<Piece> {
    let mut pieces = vec![
        Piece::tinted(Token::Success, theme::DONE.to_string()),
        Piece::plain(" "),
        Piece::tinted(Token::Text, flatten(name)),
    ];
    if let Some(version) = version.filter(|version| !version.is_empty()) {
        pieces.push(Piece::plain(" "));
        pieces.push(Piece::tinted(Token::TextDim, flatten(version)));
    }
    pieces
}

/// `✗ <name> [ failed ] <reason>`: the mark and the pill label carry the
/// failure, the brackets and the reason step back.
fn failed_pieces(name: &str, message: &str) -> Vec<Piece> {
    let mut pieces = vec![
        Piece::tinted(Token::Danger, theme::FAILED.to_string()),
        Piece::plain(" "),
        Piece::tinted(Token::Text, flatten(name)),
        Piece::plain(" "),
        Piece::tinted(Token::TextDim, "["),
        Piece::emphasized(Token::Danger, " failed "),
        Piece::tinted(Token::TextDim, "]"),
    ];
    let message = flatten(message);
    if !message.is_empty() {
        pieces.push(Piece::plain(" "));
        pieces.push(Piece::tinted(Token::TextDim, message));
    }
    pieces
}

/// A download row: spinner, name, and either a bar with a percentage and the
/// byte count, or just the byte count while the total is unknown.
fn download_pieces(row: &Row, frame: usize) -> Vec<Piece> {
    let mut pieces = vec![
        Piece::tinted(Token::Accent, spinner(frame).to_string()),
        Piece::plain(" "),
        Piece::tinted(Token::Text, &row.name),
    ];
    match row.total.filter(|total| *total > 0) {
        Some(total) => {
            let filled = (row.bytes.saturating_mul(BAR_WIDTH) / total).min(BAR_WIDTH) as usize;
            let percent = row.bytes.saturating_mul(100) / total;
            pieces.push(Piece::plain(" "));
            if filled > 0 {
                pieces.push(Piece::tinted(
                    Token::Accent,
                    theme::BAR_FULL.to_string().repeat(filled),
                ));
            }
            if filled < BAR_WIDTH as usize {
                pieces.push(Piece::tinted(
                    Token::TextFaint,
                    theme::BAR_TRACK
                        .to_string()
                        .repeat(BAR_WIDTH as usize - filled),
                ));
            }
            pieces.push(Piece::plain(" "));
            pieces.push(Piece::tinted(Token::Text, format!("{percent}%")));
            pieces.push(Piece::plain(" "));
            pieces.push(Piece::tinted(
                Token::TextDim,
                format!("{} / {}", fmt_bytes(row.bytes), fmt_bytes(total)),
            ));
        }
        None => {
            pieces.push(Piece::plain(" "));
            pieces.push(Piece::tinted(Token::TextDim, fmt_bytes(row.bytes)));
        }
    }
    pieces
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

    /// The progress unit tests share the process-global live-reporter registry,
    /// so two reporters created on parallel test threads settle each other's
    /// blocks. One lock serializes them; the gate's nextest runs one process per
    /// test and never needs it.
    static TEST_LOCK: ParkingMutex<()> = ParkingMutex::new(());

    fn progress_test_lock() -> parking_lot::MutexGuard<'static, ()> {
        TEST_LOCK.lock()
    }

    /// A progress row must be found by the event's `id`, so a component's
    /// chunks land on its own row — two components sharing a display name keep
    /// two rows — it renders the display `name`, and a finished component
    /// leaves the block.
    #[test]
    fn rows_are_keyed_by_id_and_an_outcome_removes_the_row() {
        let _guard = progress_test_lock();
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
        {
            let state = reporter.inner.state.lock();
            assert_eq!(state.rows.len(), 1);
            assert_eq!(state.rows[0].id, "FakeScheme.colors");
            assert_eq!(state.rows[0].name, "Fake Scheme");
            assert_eq!(
                state.rows[0].bytes, 50,
                "chunks land on the row keyed by id"
            );
        }
        reporter.event(&Event::Installed {
            name: "Fake Scheme".to_string(),
            id: "FakeScheme.colors".to_string(),
            version: "2.0".to_string(),
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

        let state = reporter.inner.state.lock();
        assert_eq!(state.rows.len(), 2, "the finished row must leave the block");
        assert_eq!(state.rows[0].id, "first");
        assert_eq!(state.rows[0].name, "Twin");
        assert_eq!(state.rows[1].id, "second");
    }

    /// Only one reporter is live at a time: a new reporter settles the one
    /// already drawing, so two blocks can never fight over the same rows.
    #[test]
    fn a_new_reporter_settles_the_previous_one() {
        let _guard = progress_test_lock();
        let first = ProgressReporter::new();
        let second = ProgressReporter::new();
        assert!(
            first.inner.state.lock().finished,
            "the previous reporter kept drawing"
        );
        drop(first);
        drop(second);
    }

    /// An event arriving after the reporter settled opens a fresh block below
    /// what was printed since, never a redraw over the settled block.
    #[test]
    fn an_event_after_settling_starts_a_fresh_block() {
        let _guard = progress_test_lock();
        let reporter = ProgressReporter::new();
        reporter.event(&Event::Downloading {
            name: "Old".to_string(),
            id: "old".to_string(),
            bytes: 1,
            total: None,
        });
        reporter.inner.settle();
        assert!(reporter.inner.state.lock().finished);

        reporter.event(&Event::Downloading {
            name: "New".to_string(),
            id: "new".to_string(),
            bytes: 1,
            total: None,
        });

        let state = reporter.inner.state.lock();
        assert!(!state.finished, "the settled block was never reopened");
        assert_eq!(state.drawn, 0, "the fresh block kept the settled row count");
        assert_eq!(
            state
                .rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["new"],
            "the settled block's rows were redrawn below"
        );
    }

    /// A name or message carrying a newline is flattened before it reaches a
    /// redrawn line, so the line still takes exactly one terminal row.
    #[test]
    fn a_newline_in_a_name_or_message_is_flattened() {
        let _guard = progress_test_lock();
        let line = failed_pieces("Fake\nScheme", "bad\r\nreason");
        let text: String = line.iter().map(|piece| piece.text.as_str()).collect();
        assert_eq!(text, "\u{2717} Fake Scheme [ failed ] bad  reason");
        assert!(!text.contains('\n'));
        assert!(!text.contains('\r'));
    }

    /// A line wider than the terminal is clipped to it with a trailing `…`, so
    /// the redraw's line count matches the terminal rows it owns.
    #[test]
    fn a_wide_line_is_clipped_to_the_terminal_width() {
        let _guard = progress_test_lock();
        let reporter = ProgressReporter::new();
        let pieces = failed_pieces("Fake Scheme", "checksum mismatch: expected 0000");
        let text = reporter.inner.line_text(&pieces, 30);
        let visible: String = strip_escapes(&text);
        assert_eq!(cells(&visible), 30, "{visible:?}");
        assert!(visible.ends_with('\u{2026}'), "{visible:?}");
    }

    /// A clipped line holding an emoji presentation sequence (base character +
    /// variation selector) counts it the way the string width does, so the cut
    /// never spills one cell past the width.
    #[test]
    fn a_clipped_line_counts_an_emoji_like_the_string_width() {
        let _guard = progress_test_lock();
        let reporter = ProgressReporter::new();
        let pieces = [Piece::plain("\u{2764}\u{fe0f}X")];
        let text = reporter.inner.line_text(&pieces, 2);
        let visible = strip_escapes(&text);
        assert_eq!(cells(&visible), 2, "{visible:?}");
        assert!(visible.ends_with('\u{2026}'), "{visible:?}");
    }

    /// A `Checked` event ends the checking line and settles the block, so a host
    /// that keeps the reporter alive past a store error has no spinner left.
    #[test]
    fn checked_closes_the_checking_line_and_settles() {
        let _guard = progress_test_lock();
        let reporter = ProgressReporter {
            inner: Arc::new(Inner {
                state: ParkingMutex::new(State::default()),
                colored: false,
                animated: true,
                spinning: AtomicBool::new(false),
                // The animated render writes to a buffer, so a cursor move can
                // never erase a line of the test runner's own stderr.
                out: ParkingMutex::new(Box::new(Vec::new())),
            }),
        };
        reporter.inner.state.lock().checking = Some(1);
        reporter.event(&Event::Checked);
        let state = reporter.inner.state.lock();
        assert!(
            state.checking.is_none(),
            "the checking line stayed live after Checked"
        );
        assert!(state.finished, "the reporter did not settle after Checked");
    }

    /// A width change erases no row above the block: the previous frame stays
    /// where it is and the next block starts below it, so finished rows and log
    /// lines above survive even on a terminal that truncates instead of
    /// reflowing after a resize.
    #[test]
    fn a_width_change_erases_no_row_above_the_block() {
        let state = State {
            drawn: 3,
            last_width: Some(80),
            ..State::default()
        };
        assert_eq!(
            erase_rows(&state, 80),
            3,
            "an unchanged width erases the block's own rows"
        );
        assert_eq!(
            erase_rows(&state, 40),
            0,
            "a width change must not erase upward"
        );
    }

    /// The `+N more` line reports every hidden row: the summary itself takes one
    /// of the rows the block fits, so `capacity - 1` rows stay visible and the
    /// rest are hidden.
    #[test]
    fn the_more_line_reports_every_hidden_row() {
        // At height 2 the block fits one row, all of it the summary: three
        // in-progress rows are all hidden.
        assert_eq!(rows_hidden(3, 1), 3);
        // At height 3 the block fits two rows: one stays visible and the other
        // two are hidden behind the summary.
        assert_eq!(rows_hidden(3, 2), 2);
        // At height 5 the block fits four rows: three stay visible and three of
        // the six in-progress rows are hidden.
        assert_eq!(rows_hidden(6, 4), 3);
    }

    /// A `Write` sink that records its bytes, so a test can read what an
    /// animated render emitted without touching the runner's stderr.
    struct Buffer(Arc<ParkingMutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A redraw erases the block row by row with EL, never with ED to the end
    /// of the screen: tmux's `scroll-on-clear` copies the screen into history
    /// when an ED starts at the home row, one spinner or bar frame per tick.
    #[test]
    fn an_animated_redraw_never_erases_to_the_end_of_the_screen() {
        let _guard = progress_test_lock();
        let bytes = Arc::new(ParkingMutex::new(Vec::new()));
        let reporter = ProgressReporter {
            inner: Arc::new(Inner {
                state: ParkingMutex::new(State::default()),
                colored: false,
                animated: true,
                spinning: AtomicBool::new(false),
                out: ParkingMutex::new(Box::new(Buffer(Arc::clone(&bytes)))),
            }),
        };
        {
            let mut state = reporter.inner.state.lock();
            state.drawn = 2;
            state.last_width = Some(80);
            state.rows.push(Row {
                id: "x".to_string(),
                name: "X".to_string(),
                bytes: 0,
                total: Some(1),
            });
            reporter.inner.render(&mut state);
        }
        let emitted = bytes.lock();
        assert!(
            !emitted
                .windows(3)
                .any(|window| window == b"\x1b[J".as_slice()),
            "a redraw erased to the end of the screen: {:?}",
            String::from_utf8_lossy(&emitted)
        );
        assert!(
            emitted
                .windows(4)
                .any(|window| window == b"\x1b[2K".as_slice()),
            "the block's own rows were not cleared row by row: {:?}",
            String::from_utf8_lossy(&emitted)
        );
    }

    /// Strips the SGR sequences `line_text` paints, leaving what the terminal
    /// shows.
    fn strip_escapes(text: &str) -> String {
        let mut plain = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch != '\u{1b}' {
                plain.push(ch);
                continue;
            }
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        plain
    }
}
