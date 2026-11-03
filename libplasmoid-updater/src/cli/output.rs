// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{IsTerminal as _, Write as _};

use bytesize::ByteSize;
use comfy_table::{Cell, CellAlignment, Table, presets};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::{self, Token};
use super::{Stream, styled};
use crate::{
    UpdateResult,
    types::{AvailableUpdate, InstalledComponent},
};

/// The `[  ]` around a failure kind's pill, spaces included.
const PILL_BRACKETS: usize = 4;

/// The narrowest ERROR column a wrapped failure table uses, so a long name and
/// kind cannot squeeze the message to a column of one character.
const MIN_ERROR_WIDTH: usize = 20;

/// The cells `text` occupies on a terminal: a wide (CJK) character takes two.
pub(crate) fn cells(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// `text` followed by spaces until it spans `width` cells.
pub(crate) fn pad_right(text: &str, width: usize) -> String {
    let mut padded = text.to_string();
    padded.push_str(&" ".repeat(width.saturating_sub(cells(text))));
    padded
}

/// Spaces followed by `text`, so its right edge sits at `width` cells.
pub(crate) fn pad_left(text: &str, width: usize) -> String {
    let mut padded = " ".repeat(width.saturating_sub(cells(text)));
    padded.push_str(text);
    padded
}

pub(crate) fn format_version(version: &str) -> &str {
    if version.is_empty() || version == "0.0.0" {
        "N/A"
    } else {
        version
    }
}

fn header(name: &str) -> Cell {
    Cell::new(name).fg(theme::table_colour(Token::TextDim))
}

fn right(value: &str) -> Cell {
    Cell::new(value).set_alignment(CellAlignment::Right)
}

trait TableRow {
    fn to_row(&self) -> Vec<Cell>;
}

impl TableRow for AvailableUpdate {
    fn to_row(&self) -> Vec<Cell> {
        vec![
            Cell::new(&self.installed.name),
            right(format_version(&self.installed.version)),
            right(format_version(&self.latest_version)),
            right(&self.content_id.to_string()),
            right(&format_download_size(self.download_size)),
            Cell::new(self.installed.component_type.to_string()),
        ]
    }
}

impl TableRow for InstalledComponent {
    fn to_row(&self) -> Vec<Cell> {
        vec![
            Cell::new(&self.name),
            right(format_version(&self.version)),
            Cell::new(self.component_type.to_string()),
        ]
    }
}

/// Prints a table of failed components to **stderr**: one row per failure
/// with the component name, the failed stage as a pill, and the message.
///
/// On a terminal the ERROR cell wraps inside its column, continuation lines
/// indented to the column, so the message never wraps at the terminal edge back
/// to the first column. Piped or width-unknown output keeps each message on its
/// own line.
pub fn print_failures(result: &UpdateResult) {
    if result.failed.is_empty() {
        return;
    }

    super::progress::settle_live();

    let colored = styled(Stream::Stderr);
    let error_width = std::io::stderr()
        .is_terminal()
        .then(|| super::progress::terminal_width().unwrap_or(super::progress::DEFAULT_WIDTH));
    let table = failure_table(result, colored, error_width);

    let mut err = std::io::stderr().lock();
    let _ = err.write_all(table.as_bytes());
    let _ = err.flush();
}

/// Renders the failure table. On a terminal the ERROR cell wraps inside its
/// column; piped or width-unknown output splits a multi-line message on its own
/// newlines, so a continuation line stays indented to the column.
fn failure_table(result: &UpdateResult, colored: bool, error_width: Option<usize>) -> String {
    let dim = |text: String| {
        if colored {
            theme::paint(Token::TextDim, &text)
        } else {
            text
        }
    };
    let kinds: Vec<String> = result
        .failed
        .iter()
        .map(|failure| failure.kind.to_string())
        .collect();
    let name_width = result
        .failed
        .iter()
        .map(|failure| cells(&failure.component.name))
        .chain(std::iter::once("NAME".len()))
        .max()
        .unwrap_or_default();
    let kind_width = kinds
        .iter()
        .map(|kind| cells(kind) + PILL_BRACKETS)
        .chain(std::iter::once("KIND".len()))
        .max()
        .unwrap_or_default();
    let error_column = name_width + 2 + kind_width + 2;
    let error_width =
        error_width.map(|width| width.saturating_sub(error_column).max(MIN_ERROR_WIDTH));

    let mut table = String::new();
    table.push_str(&format!(
        "{}  {}  {}\n",
        dim(pad_right("NAME", name_width)),
        dim(pad_right("KIND", kind_width)),
        dim("ERROR".to_string()),
    ));
    for (failure, kind) in result.failed.iter().zip(&kinds) {
        let pill = if colored {
            format!(
                "{}{}{}",
                theme::paint(Token::TextDim, "["),
                theme::paint_bold(Token::Danger, &format!(" {kind} ")),
                theme::paint(Token::TextDim, "]"),
            )
        } else {
            format!("[ {kind} ]")
        };
        let padding = " ".repeat(kind_width.saturating_sub(cells(kind) + PILL_BRACKETS));
        let message = match error_width {
            Some(width) => wrap(&failure.message, width),
            None => failure.message.lines().map(str::to_string).collect(),
        };
        let mut lines = message.into_iter();
        table.push_str(&format!(
            "{}  {pill}{padding}  {}\n",
            pad_right(&failure.component.name, name_width),
            lines.next().unwrap_or_default(),
        ));
        for line in lines {
            table.push_str(&format!("{}{line}\n", " ".repeat(error_column)));
        }
    }
    table
}

/// Breaks `text` into lines of at most `width` cells, at spaces where one fits
/// and inside a word only when the word alone is wider than the column.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_cells = 0;
    for word in text.split_whitespace() {
        let word_cells = cells(word);
        if line_cells > 0 {
            if line_cells + 1 + word_cells <= width {
                line.push(' ');
                line.push_str(word);
                line_cells += 1 + word_cells;
                continue;
            }
            lines.push(std::mem::take(&mut line));
            line_cells = 0;
        }
        if word_cells <= width {
            line.push_str(word);
            line_cells = word_cells;
            continue;
        }
        for ch in word.chars() {
            let cell_width = UnicodeWidthChar::width(ch).unwrap_or_default();
            if line_cells + cell_width > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line_cells = 0;
            }
            line.push(ch);
            line_cells += cell_width;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn format_download_size(size: Option<u64>) -> String {
    size.map(|b| ByteSize(b).to_string())
        .unwrap_or_else(|| "-".to_string())
}

/// Renders the run's summary line: one `updated <n>`, `failed <n>` and
/// `skipped <n>` per non-zero count, joined by ` · `. A zero count is
/// omitted, so a clean run reads `updated 1`.
///
/// The counts are coloured for `stream` when [`styled`] says that stream takes
/// styling, and plain text otherwise.
///
/// # Examples
///
/// ```
/// use libplasmoid_updater::{UpdateResult, cli};
///
/// let mut result = UpdateResult::default();
/// result.skipped = vec!["widget".to_string()];
/// assert_eq!(
///     cli::update_summary(&result, cli::Stream::Stdout),
///     "skipped 1",
/// );
/// ```
pub fn update_summary(result: &UpdateResult, stream: Stream) -> String {
    let colored = styled(stream);
    let counts = [
        (result.succeeded.len(), "updated", Token::Success),
        (result.failed.len(), "failed", Token::Danger),
        (result.skipped.len(), "skipped", Token::TextDim),
    ];
    let parts: Vec<String> = counts
        .iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, label, token)| {
            if colored {
                format!(
                    "{} {}",
                    theme::paint(Token::TextDim, label),
                    theme::paint(*token, &count.to_string()),
                )
            } else {
                format!("{label} {count}")
            }
        })
        .collect();
    let separator = if colored {
        theme::paint(Token::TextDim, " \u{00b7} ")
    } else {
        " \u{00b7} ".to_string()
    };
    parts.join(&separator)
}

/// Decides styling from the stream the table ends up on, so the header colour
/// follows the same predicate as every other terminal surface rather than
/// comfy-table's own TTY probe.
fn apply_styling(table: &mut Table, stream: Stream) {
    if styled(stream) {
        table.enforce_styling();
    } else {
        table.force_no_tty();
    }
}

fn render_table<T: TableRow>(items: &[T], headers: &[&str], stream: Stream) -> String {
    let mut table = Table::new();
    table.load_style(presets::NOTHING);
    apply_styling(&mut table, stream);
    table.set_header(headers.iter().map(|h| header(h)).collect::<Vec<_>>());

    for item in items {
        table.add_row(item.to_row());
    }

    table.to_string()
}

/// Renders the table of available updates (`check`'s payload), styled for
/// `stream`, the stream the caller prints it on.
pub fn updates_table(updates: &[AvailableUpdate], stream: Stream) -> String {
    let headers = ["NAME", "CURRENT", "AVAILABLE", "ID", "SIZE", "TYPE"];
    render_table(updates, &headers, stream)
}

/// Renders the table of installed components (`list-installed`'s payload),
/// styled for `stream`, the stream the caller prints it on.
pub fn components_table(components: &[InstalledComponent], stream: Stream) -> String {
    let headers = ["NAME", "VERSION", "TYPE"];
    render_table(components, &headers, stream)
}

#[cfg(test)]
mod tests {
    use super::{failure_table, wrap};
    use crate::{ComponentType, Failure, FailureKind, InstalledComponent, UpdateResult};

    /// A message breaks at a space once the next word no longer fits.
    #[test]
    fn wraps_a_message_inside_its_column() {
        assert_eq!(
            wrap("checksum mismatch: expected abc", 20),
            vec!["checksum mismatch:".to_string(), "expected abc".to_string()]
        );
    }

    /// A single word wider than the column splits across lines rather than
    /// overflowing the terminal edge.
    #[test]
    fn splits_a_word_wider_than_the_column() {
        assert_eq!(
            wrap("abcdefghij", 4),
            vec!["abcd".to_string(), "efgh".to_string(), "ij".to_string()]
        );
    }

    /// A piped failure table splits a multi-line message on its own newlines,
    /// so the continuation line stays indented to the ERROR column instead of
    /// landing at column 0.
    #[test]
    fn a_piped_failure_table_indents_a_multi_line_message() {
        let result = UpdateResult {
            succeeded: Vec::new(),
            failed: vec![Failure {
                component: InstalledComponent {
                    name: "Fake Scheme".to_string(),
                    directory_name: "FakeScheme.colors".to_string(),
                    version: "1.0".to_string(),
                    component_type: ComponentType::ColorScheme,
                    path: std::path::PathBuf::from("/tmp/FakeScheme.colors"),
                    is_system: false,
                    release_date: String::new(),
                },
                kind: FailureKind::Checksum,
                message: "sudo command failed: a\nb".to_string(),
            }],
            skipped: Vec::new(),
        };
        let table = failure_table(&result, false, None);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[2].starts_with(' ') && lines[2].trim_start() == "b",
            "the message continuation landed at column 0: {lines:?}"
        );
    }
}
