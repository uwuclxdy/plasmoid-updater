// SPDX-License-Identifier: MIT OR Apache-2.0

//! CLI-specific functionality: output formatting and progress indicators.
//!
//! **This module is only available when the `cli` feature is enabled.**
//!
//! These components are separate from the core library functionality to allow
//! non-CLI consumers (like topgrade) to use the library without pulling in
//! terminal UI dependencies.

pub(crate) mod output;
pub(crate) mod progress;
pub(crate) mod update_ui;

pub(crate) const CLEAR_LINE_SEQUENCE: &str = "\x1b[1A\r\x1b[2K";
