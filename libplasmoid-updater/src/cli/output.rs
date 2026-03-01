// SPDX-License-Identifier: MIT OR Apache-2.0

use bytesize::ByteSize;
use comfy_table::{Attribute, Cell, CellAlignment, Table, presets};

use crate::{
    UpdateResult,
    types::{AvailableUpdate, InstalledComponent},
};

pub fn format_version(version: &str) -> &str {
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

impl TableRow for (String, String) {
    fn to_row(&self) -> Vec<Cell> {
        vec![Cell::new(&self.0), Cell::new(&self.1)]
    }
}

fn format_download_size(size: Option<u64>) -> String {
    size.map(|b| ByteSize(b).to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn print_table<T: TableRow>(items: &[T], headers: &[&str]) {
    let mut table = Table::new();
    table.load_preset(presets::NOTHING);
    table.set_header(headers.iter().map(|h| header(h)).collect::<Vec<_>>());

    for item in items {
        table.add_row(item.to_row());
    }

    println!("{table}");
}

pub fn print_updates_table(updates: &[AvailableUpdate]) {
    let headers = vec!["NAME", "CURRENT", "AVAILABLE", "ID", "SIZE", "TYPE"];
    print_table(updates, &headers);
}

pub fn print_components_table(components: &[InstalledComponent]) {
    let headers = vec!["NAME", "VERSION", "TYPE"];
    print_table(components, &headers);
}

pub fn print_error_table(update_result: UpdateResult) {
    let headers = vec!["NAME", "ERROR"];
    print_table(&update_result.failed, &headers);
}

pub fn print_summary(update_result: UpdateResult) {
    let total =
        update_result.succeeded.len() + update_result.failed.len() + update_result.skipped.len();
    println!(
        "Update Summary: {} succeeded, {} failed, {} skipped ({} total)",
        update_result.succeeded.len(),
        update_result.failed.len(),
        update_result.skipped.len(),
        total
    );
}

pub fn print_count_message(count: usize, item_type: &str) {
    let plural = if count == 1 { "" } else { "s" };
    println!("{} {}{} available.", count, item_type, plural);
}
