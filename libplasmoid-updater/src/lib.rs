// SPDX-FileCopyrightText: 2025 uwuclxdy
// SPDX-License-Identifier: GPL-3.0-or-later
//
// This implementation is based on:
// - Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// - KDE Discover's KNewStuff backend (https://invent.kde.org/plasma/discover) -
//   GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL
//
// The update detection algorithm, KDE Store API interaction, and widget ID resolution
// approach are derived from Apdatifier's shell scripts. The KNewStuff registry format
// and installation process knowledge comes from KDE Discover's source code.

pub(crate) mod api;
pub(crate) mod checker;
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod installer;
pub(crate) mod paths;
pub(crate) mod registry;
pub(crate) mod types;
pub(crate) mod utils;
pub(crate) mod version;

#[cfg(feature = "cli")]
pub mod cli;

use api::ApiClient;
use serde::Serialize;
use types::UpdateCheckResult;

pub use config::{Config, RestartBehavior};
pub use error::Error;
pub use types::{AvailableUpdate, ComponentType, Diagnostic, InstalledComponent};

/// A specialized `Result` type for libplasmoid-updater operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Checks for available updates to installed KDE Plasma components.
///
/// Scans the local filesystem for installed KDE components and queries the KDE Store API
/// for newer versions. Returns an empty [`CheckResult`] when no updates are found — not an error.
///
/// With the `cli` feature enabled, displays a spinner during fetch and a summary table of updates.
///
/// # Errors
///
/// - [`Error::UnsupportedOS`] — not running on Linux
/// - [`Error::NotKDE`] — KDE Plasma not detected
pub fn check(config: &Config) -> Result<CheckResult> {
    crate::utils::validate_environment(config.skip_plasma_detection)?;

    let api_client = ApiClient::new();
    let result = crate::utils::fetch_updates(&api_client, config)?;

    #[cfg(feature = "cli")]
    crate::utils::display_check_results(&result);

    Ok(CheckResult::from_internal(result))
}

/// Result of checking for available updates.
///
/// Returned by [`check()`](crate::check). Contains the full [`AvailableUpdate`] data
/// for each pending update, plus diagnostics for components that could not be checked.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Available updates found during the check.
    pub available_updates: Vec<AvailableUpdate>,
    /// Components that could not be checked, with the reason for each failure.
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckResult {
    pub(crate) fn from_internal(result: UpdateCheckResult) -> Self {
        let diagnostics = result
            .unresolved
            .into_iter()
            .chain(result.check_failures)
            .collect();

        Self {
            available_updates: result.updates,
            diagnostics,
        }
    }

    /// Returns `true` if at least one update is available.
    pub fn has_updates(&self) -> bool {
        !self.available_updates.is_empty()
    }

    /// Returns the number of available updates.
    pub fn update_count(&self) -> usize {
        self.available_updates.len()
    }

    /// Returns `true` if there are no updates and no diagnostics.
    pub fn is_empty(&self) -> bool {
        self.available_updates.is_empty() && self.diagnostics.is_empty()
    }
}

/// Downloads and installs all available updates for installed KDE Plasma components.
///
/// Runs the full update pipeline: scan installed components, check for updates, select
/// which to apply, then download and install. Handles plasmashell restart based on
/// [`Config::restart`]. Components in [`Config::excluded_packages`] are always skipped.
///
/// With the `cli` feature enabled and [`Config::auto_confirm`] unset, shows an interactive
/// multi-select menu. Otherwise, all available updates are applied automatically.
///
/// # Errors
///
/// Returns an [`Error`] if environment validation, network requests, or installation fails.
pub fn update(config: &Config) -> Result<UpdateResult> {
    crate::utils::validate_environment(config.skip_plasma_detection)?;

    let api_client = ApiClient::new();
    let check_result = crate::utils::fetch_updates(&api_client, config)?;

    if check_result.updates.is_empty() {
        #[cfg(feature = "cli")]
        println!("no updates available");

        return Ok(UpdateResult::default());
    }

    let selected = crate::utils::select_updates(&check_result.updates, config)?;

    if selected.is_empty() {
        #[cfg(feature = "cli")]
        println!("nothing to update");

        return Ok(UpdateResult::default());
    }

    let result = crate::utils::install_selected_updates(&selected, &api_client, config)?;

    #[cfg(feature = "debug")]
    {
        let n = api_client.request_count();
        let plural = if n == 1 { "" } else { "s" };
        println!("{n} web request{plural}");
    }

    crate::utils::handle_restart(config, &check_result.updates, &result);

    Ok(result)
}

/// A component that failed to update, with the error message.
#[derive(Debug, Clone, Serialize)]
pub struct FailedUpdate {
    /// Display name of the component that failed.
    pub name: String,
    /// Human-readable error description.
    pub error: String,
}

/// Result of performing updates.
///
/// Returned by [`update()`](crate::update). Tracks which components succeeded,
/// failed, or were skipped during the update run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<FailedUpdate>,
    pub skipped: Vec<String>,
}

impl UpdateResult {
    /// Returns `true` if any component failed to update.
    pub fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }

    /// Returns `true` if no update actions were attempted.
    pub fn is_empty(&self) -> bool {
        self.succeeded.is_empty() && self.failed.is_empty() && self.skipped.is_empty()
    }

    /// Returns the number of successfully updated components.
    pub fn success_count(&self) -> usize {
        self.succeeded.len()
    }

    /// Returns the number of components that failed to update.
    pub fn failure_count(&self) -> usize {
        self.failed.len()
    }

    /// Prints a formatted table of failed updates to stdout.
    #[cfg(feature = "cli")]
    pub fn print_error_table(&self) {
        crate::cli::output::print_error_table(self);
    }

    /// Prints a one-line summary of the update outcome to stdout.
    #[cfg(feature = "cli")]
    pub fn print_summary(&self) {
        crate::cli::output::print_summary(self);
    }
}

/// Returns all installed KDE Plasma components without making network requests.
///
/// Scans the filesystem and KNewStuff registry to discover locally installed components.
/// Useful for building custom UIs or auditing what is installed.
///
/// # Errors
///
/// Returns an error if the filesystem scan fails.
pub fn get_installed(config: &Config) -> Result<Vec<InstalledComponent>> {
    checker::find_installed(config.system)
}

/// Downloads and installs a single component update with automatic backup and rollback.
///
/// On failure, the original component is restored from backup. Does not handle
/// plasmashell restart — the caller is responsible for restarting if needed.
///
/// # Errors
///
/// Returns an error if download, installation, or backup operations fail.
pub fn install_update(update: &AvailableUpdate, config: &Config) -> Result<()> {
    let _ = config;
    let api_client = ApiClient::new();
    let counter = api_client.request_counter();
    installer::update_component(update, api_client.http_client(), |_| {}, &counter)
}

/// Discovers and prints all installed KDE components as a formatted table.
///
/// Scans the filesystem and KNewStuff registry without making network requests.
/// Prints a count header followed by a table of all discovered components.
///
/// # Errors
///
/// Returns an error if the filesystem scan fails.
#[cfg(feature = "cli")]
#[doc(hidden)]
pub fn show_installed(config: &Config) -> Result<()> {
    let components = checker::find_installed(config.system)?;

    if components.is_empty() {
        println!("no components installed");
        return Ok(());
    }

    cli::output::print_count_message(components.len(), "installed component");
    cli::output::print_components_table(&components);

    Ok(())
}
