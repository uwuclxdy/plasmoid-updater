// SPDX-License-Identifier: GPL-3.0-or-later

//! Terminal pieces behind the `cli` feature: a progress reporter, the
//! multi-select prompt, and failure/table printers.
//!
//! These components are separate from the core library functionality so
//! non-CLI consumers (like topgrade) can use the library without pulling in
//! terminal UI dependencies. Nothing in the core calls this module; the
//! caller passes these pieces in explicitly.

mod output;
mod progress;
mod select;

pub use output::{print_components_table, print_failures, print_updates_table};
pub use progress::ProgressReporter;
pub use select::select;

pub(crate) const CLEAR_LINE_SEQUENCE: &str = "\x1b[1A\r\x1b[2K";
