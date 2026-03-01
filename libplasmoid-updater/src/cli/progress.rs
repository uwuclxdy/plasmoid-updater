// SPDX-License-Identifier: MIT OR Apache-2.0

use indicatif::{ProgressBar, ProgressStyle};

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template(" {spinner:.cyan} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

fn create_spinner(msg: impl Into<String>) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message(msg.into());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

/// Creates a spinner for the "Fetching component data" phase.
pub fn create_fetch_spinner() -> ProgressBar {
    create_spinner("Fetching component data")
}

