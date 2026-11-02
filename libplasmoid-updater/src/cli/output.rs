// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::Write as _;

use bytesize::ByteSize;
use comfy_table::{Attribute, Cell, CellAlignment, Table, presets};

use crate::{
    UpdateResult,
    types::{AvailableUpdate, InstalledComponent},
};

pub(crate) fn format_version(version: &str) -> &str {
    if version.is_empty() || version == "0.0.0" {
        "N/A"
    } else {
        version
    }
}

fn header(name: &str) -> Cell {
    Cell::new(name).add_attribute(Attribute::Bold)
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
/// with the component name, the failed stage and the message.
pub fn print_failures(result: &UpdateResult) {
    if result.failed.is_empty() {
        return;
    }

    // The table goes to stderr, so comfy-table must decide styling from the
    // stderr stream's TTY state, not stdout's.
    let mut table = Table::new();
    table.use_stderr().load_style(presets::NOTHING);
    table.set_header(vec![header("NAME"), header("KIND"), header("ERROR")]);

    for failure in &result.failed {
        table.add_row(vec![
            Cell::new(&failure.component.name),
            Cell::new(failure.kind.to_string()),
            Cell::new(&failure.message),
        ]);
    }

    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{table}");
    let _ = err.flush();
}

fn format_download_size(size: Option<u64>) -> String {
    size.map(|b| ByteSize(b).to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn render_table<T: TableRow>(items: &[T], headers: &[&str]) -> String {
    let mut table = Table::new();
    table.load_style(presets::NOTHING);
    table.set_header(headers.iter().map(|h| header(h)).collect::<Vec<_>>());

    for item in items {
        table.add_row(item.to_row());
    }

    table.to_string()
}

/// Renders the table of available updates (`check`'s payload).
pub fn updates_table(updates: &[AvailableUpdate]) -> String {
    let headers = ["NAME", "CURRENT", "AVAILABLE", "ID", "SIZE", "TYPE"];
    render_table(updates, &headers)
}

/// Renders the table of installed components (`list-installed`'s payload).
pub fn components_table(components: &[InstalledComponent]) -> String {
    let headers = ["NAME", "VERSION", "TYPE"];
    render_table(components, &headers)
}
