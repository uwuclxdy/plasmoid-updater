// SPDX-License-Identifier: MIT OR Apache-2.0

use indicatif::{ProgressBar, ProgressStyle};

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.cyan} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_spinner())
}

/// Creates a spinner for the "Fetching component data" phase.
pub fn create_fetch_spinner() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message("Fetching component data");
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

/// Creates a spinner for updating a specific component.
pub fn create_component_spinner(name: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style());
    pb.set_message(format!("Updating {}", name));
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

/// Prints a success status line for a component update.
pub fn print_update_success(name: &str, old_version: &str, new_version: &str) {
    print!(
        "\x1b[1A\x1b[2K\r  \u{2713} {} ({} \u{2192} {})\n",
        name, old_version, new_version
    );
}

/// Prints a failure status line for a component update.
pub fn print_update_failure(name: &str) {
    print!("\x1b[1A\x1b[2K\r  \u{2717} {} (failed)\n", name);
}
