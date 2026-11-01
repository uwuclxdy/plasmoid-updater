// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{IsTerminal, Write as _};

use inquire::InquireError;

use super::CLEAR_LINE_SEQUENCE;
use crate::{Error, Result, types::AvailableUpdate};

/// The multi-select prompt over available updates.
///
/// Every offered update is pre-checked; the caller filters the list it passes
/// if it wants fewer options offered. Cancel and Ctrl-C return an empty
/// selection, which the caller can treat as "change nothing".
///
/// # Errors
///
/// Returns [`Error::Config`] when `updates` is non-empty and stdin is not a
/// terminal (the prompt would otherwise hang a non-interactive caller), and
/// when the prompt itself fails. An empty `updates` returns an empty selection
/// without touching the terminal.
pub fn select(updates: &[AvailableUpdate]) -> Result<Vec<AvailableUpdate>> {
    if updates.is_empty() {
        return Ok(Vec::new());
    }

    if !std::io::stdin().is_terminal() {
        return Err(Error::Config(
            "interactive selection requires a terminal on stdin; \
             pass a preselected list to `install` instead"
                .into(),
        ));
    }

    let options = format_menu_options(updates);
    let prompt = format!(
        "{} update{} available, select to apply:",
        updates.len(),
        if updates.len() == 1 { "" } else { "s" }
    );

    match inquire::MultiSelect::new(&prompt, options)
        .with_default(&(0..updates.len()).collect::<Vec<usize>>())
        .with_page_size(15)
        .raw_prompt()
    {
        Ok(selected) => {
            let mut stderr = std::io::stderr();
            if stderr.is_terminal() {
                let _ = write!(stderr, "{CLEAR_LINE_SEQUENCE}");
                let _ = stderr.flush();
            }
            Ok(selected
                .into_iter()
                .map(|opt| updates[opt.index].clone())
                .collect())
        }
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(Vec::new()),
        Err(e) => Err(Error::Config(format!("selection prompt failed: {e}"))),
    }
}

fn format_menu_options(updates: &[AvailableUpdate]) -> Vec<String> {
    let name_width = updates
        .iter()
        .map(|u| u.installed.name.len())
        .max()
        .unwrap_or(10)
        .max(10);

    updates
        .iter()
        .map(|u| {
            format!(
                "{:<name_width$} {} \u{2192} {}",
                u.installed.name,
                super::output::format_version(&u.installed.version),
                super::output::format_version(&u.latest_version),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ComponentType, InstalledComponent};
    use std::path::PathBuf;

    fn update(name: &str, from: &str, to: &str) -> AvailableUpdate {
        AvailableUpdate {
            installed: InstalledComponent {
                name: name.to_string(),
                directory_name: name.to_string(),
                version: from.to_string(),
                component_type: ComponentType::PlasmaWidget,
                path: PathBuf::from("/tmp/test"),
                is_system: false,
                release_date: String::new(),
            },
            content_id: 1,
            latest_version: to.to_string(),
            download_url: String::new(),
            store_url: String::new(),
            release_date: String::new(),
            checksum: None,
            download_size: None,
        }
    }

    #[test]
    fn menu_options_align_names_and_show_both_versions() {
        let options =
            format_menu_options(&[update("B", "1.0", "2.0"), update("Aaa", "3.0", "4.0")]);
        assert_eq!(options.len(), 2);
        // names pad to the 10-char minimum width
        assert!(
            options[0].starts_with("B          1.0 \u{2192} 2.0"),
            "{}",
            options[0]
        );
        assert!(
            options[1].starts_with("Aaa        3.0 \u{2192} 4.0"),
            "{}",
            options[1]
        );
    }
}
