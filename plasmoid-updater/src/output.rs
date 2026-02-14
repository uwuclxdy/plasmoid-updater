// SPDX-License-Identifier: MIT OR Apache-2.0

use bytesize::ByteSize;
use comfy_table::{Attribute, Cell, CellAlignment, Table, presets};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::exit_code::ExitCode;
use libplasmoid_updater::{AvailableUpdate, InstalledComponent};

/// Verbosity level for CLI output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

impl std::fmt::Display for Verbosity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quiet => write!(f, "quiet"),
            Self::Normal => write!(f, "normal"),
            Self::Verbose => write!(f, "verbose"),
        }
    }
}

/// Generic JSON output wrapper for CLI responses.
#[derive(Debug, Serialize)]
pub struct JsonOutput<T> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> JsonOutput<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            error: None,
            data: Some(data),
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            error: Some(msg.into()),
            data: None,
        }
    }
}

pub fn output_error(json: bool, msg: &str) {
    if json {
        let output: JsonOutput<()> = JsonOutput::err(msg);
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        eprintln!("{} {msg}", "error:".bold());
    }
}

pub fn format_version(version: &str) -> &str {
    if version.is_empty() { "N/A" } else { version }
}

fn header(name: &str) -> Cell {
    Cell::new(name).add_attribute(Attribute::Bold)
}

fn right(value: &str) -> Cell {
    Cell::new(value).set_alignment(CellAlignment::Right)
}

trait TableRow {
    fn to_row(&self, verbose: bool) -> Vec<Cell>;
}

impl TableRow for AvailableUpdate {
    fn to_row(&self, verbose: bool) -> Vec<Cell> {
        let mut row = vec![
            Cell::new(&self.installed.name),
            right(format_version(&self.installed.version)),
            right(format_version(&self.latest_version)),
        ];
        if verbose {
            row.push(right(&self.content_id.to_string()));
            row.push(right(&format_download_size(self.download_size)));
            row.push(Cell::new(self.installed.component_type.to_string()));
        }
        row
    }
}

impl TableRow for InstalledComponent {
    fn to_row(&self, verbose: bool) -> Vec<Cell> {
        let mut row = vec![Cell::new(&self.name), right(format_version(&self.version))];
        if verbose {
            row.push(Cell::new(self.component_type.to_string()));
        }
        row
    }
}

fn format_download_size(size: Option<u64>) -> String {
    size.map(|b| ByteSize(b).to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn print_table<T: TableRow>(items: &[T], headers: &[&str], verbose: bool) {
    let mut table = Table::new();
    table.load_preset(presets::NOTHING);
    table.set_header(headers.iter().map(|h| header(h)).collect::<Vec<_>>());

    for item in items {
        table.add_row(item.to_row(verbose));
    }

    println!("{table}");
}

pub fn print_updates_table(updates: &[AvailableUpdate], verbosity: Verbosity) {
    let headers = if verbosity == Verbosity::Verbose {
        vec!["NAME", "CURRENT", "AVAILABLE", "ID", "SIZE", "TYPE"]
    } else {
        vec!["NAME", "CURRENT", "AVAILABLE"]
    };
    print_table(updates, &headers, verbosity == Verbosity::Verbose);
}

pub fn print_components_table(components: &[InstalledComponent], verbosity: Verbosity) {
    let headers = if verbosity == Verbosity::Verbose {
        vec!["NAME", "VERSION", "TYPE"]
    } else {
        vec!["NAME", "VERSION"]
    };
    print_table(components, &headers, verbosity == Verbosity::Verbose);
}

pub fn output_json<T: Serialize>(data: T) -> Result<ExitCode, libplasmoid_updater::Error> {
    let output = JsonOutput::ok(&data);
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(ExitCode::Success)
}

pub fn output_json_error(msg: &str) -> Result<ExitCode, libplasmoid_updater::Error> {
    let output: JsonOutput<()> = JsonOutput::err(msg);
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(ExitCode::PartialFailure)
}

pub fn print_info(verbosity: Verbosity, msg: &str) {
    if verbosity != Verbosity::Quiet {
        println!("{} {}", "info:".bold(), msg);
    }
}

pub fn print_count_message(verbosity: Verbosity, count: usize, item_type: &str) {
    if verbosity == Verbosity::Quiet {
        println!("{}", count);
    } else {
        let plural = if count == 1 { "" } else { "s" };
        println!(
            "{} {} {}{} available:",
            "info:".bold(),
            count,
            item_type,
            plural
        );
        println!();
    }
}

pub fn print_fatal_error(msg: &str) {
    eprintln!("{} {msg}", "fatal error:".bold());
}

pub fn print_non_interactive_hint(update_count: usize) {
    println!();
    let plural = if update_count == 1 { "" } else { "s" };
    println!(
        "run with --yes to update all, or specify name{} to update",
        plural
    );
}
