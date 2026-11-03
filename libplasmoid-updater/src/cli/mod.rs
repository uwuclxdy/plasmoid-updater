// SPDX-License-Identifier: GPL-3.0-or-later

//! Terminal pieces behind the `cli` feature: a progress reporter, the
//! multi-select prompt, failure/table printers, the run summary, the path a
//! log line takes above the live progress, and the styling predicate they all
//! share.
//!
//! These components are separate from the core library functionality so
//! non-CLI consumers (like topgrade) can use the library without pulling in
//! terminal UI dependencies. Nothing in the core calls this module; the
//! caller passes these pieces in explicitly.

mod output;
mod progress;
mod select;
mod theme;

pub use output::{components_table, print_failures, update_summary, updates_table};
pub use progress::{ProgressReporter, print_above};
pub use select::select;

/// Which stream a piece of terminal output is drawn on.
///
/// The styling predicate reads a stream's own terminal state, so the caller
/// must name where the output ends up: a table this module renders to a
/// `String` goes wherever the caller prints it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output, the payload stream.
    Stdout,
    /// Standard error, the diagnostics and progress stream.
    Stderr,
}

/// Returns `true` when output drawn on `stream` should carry styling.
///
/// The rule is the one `anstream` applies to clap's `--help`: `NO_COLOR` set
/// non-empty turns styling off; otherwise `CLICOLOR_FORCE` set non-empty turns
/// it on, even when the stream is not a terminal; otherwise `CLICOLOR=0` turns
/// it off; otherwise it is on only when the stream is a terminal, and there
/// only when `TERM` is set and not `dumb`, `CLICOLOR` is set to anything but
/// `0`, or `CI` is set. A process-wide choice set through
/// `anstream::ColorChoice::write_global` (a host's own `--color` flag) wins over
/// all of these.
///
/// # Examples
///
/// ```no_run
/// use libplasmoid_updater::cli::{Stream, styled};
///
/// let colour = styled(Stream::Stdout);
/// ```
pub fn styled(stream: Stream) -> bool {
    let choice = match stream {
        Stream::Stdout => anstream::AutoStream::choice(&std::io::stdout()),
        Stream::Stderr => anstream::AutoStream::choice(&std::io::stderr()),
    };
    choice != anstream::ColorChoice::Never
}

pub(crate) const CLEAR_LINE_SEQUENCE: &str = "\x1b[1A\r\x1b[2K";
