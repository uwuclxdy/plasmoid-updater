// SPDX-License-Identifier: MIT OR Apache-2.0
//
// This implementation is based on:
// - Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// - KDE Discover's KNewStuff backend (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+
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
use types::UpdateCheckResult;

pub use config::{Config, RestartBehavior};
pub use error::Error;
pub use types::{AvailableUpdate, ComponentType, InstalledComponent};

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
/// - [`CheckError::UnsupportedOS`] — not running on Linux
/// - [`CheckError::NotKDE`] — KDE Plasma not detected
/// - [`CheckError::Other`] — unexpected failure
pub fn check(config: &Config) -> std::result::Result<CheckResult, CheckError> {
    crate::utils::validate_environment()?;

    let api_client = ApiClient::new();
    let result = crate::utils::fetch_updates(&api_client, config)?;

    #[cfg(feature = "cli")]
    crate::utils::display_check_results(&result);

    Ok(CheckResult::from_internal(&result))
}

/// Result of checking for available updates.
///
/// Returned by [`check()`](crate::check). Summarizes available updates and lists any
/// components that could not be checked, along with the reason for each failure.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Available updates found during the check.
    pub available_updates: Vec<AvailableUpdateInfo>,
    /// Components that could not be checked (name, reason).
    pub diagnostics: Vec<(String, String)>,
}

impl CheckResult {
    pub(crate) fn from_internal(result: &UpdateCheckResult) -> Self {
        let available_updates = result
            .updates
            .iter()
            .map(AvailableUpdateInfo::from_internal)
            .collect();

        let diagnostics = result
            .unresolved
            .iter()
            .chain(&result.check_failures)
            .map(|d| (d.name.clone(), d.reason.clone()))
            .collect();

        Self {
            available_updates,
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
}

/// Describes a single installed component that has an available update.
///
/// Returned as part of [`CheckResult::available_updates`]. Contains version
/// info and metadata needed to identify and display the pending update.
#[derive(Debug, Clone)]
pub struct AvailableUpdateInfo {
    pub name: String,
    pub directory_name: String,
    pub current_version: String,
    pub available_version: String,
    pub component_type: String,
    pub content_id: u64,
    pub download_size: Option<u64>,
}

impl AvailableUpdateInfo {
    fn from_internal(update: &AvailableUpdate) -> Self {
        Self {
            name: update.installed.name.clone(),
            directory_name: update.installed.directory_name.clone(),
            current_version: update.installed.version.clone(),
            available_version: update.latest_version.clone(),
            component_type: update.installed.component_type.to_string(),
            content_id: update.content_id,
            download_size: update.download_size,
        }
    }
}

/// Error returned by [`check()`](crate::check) during the detection phase.
#[derive(Debug)]
pub enum CheckError {
    /// The current operating system is not supported.
    UnsupportedOS(String),
    /// The desktop environment is not KDE Plasma.
    NotKDE,
    /// An unexpected error occurred during detection.
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedOS(os) => write!(f, "unsupported operating system: {os}"),
            Self::NotKDE => write!(f, "KDE Plasma desktop environment not detected"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Other(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<Error> for CheckError {
    fn from(e: Error) -> Self {
        Self::Other(Box::new(e))
    }
}

/// Downloads and installs all available updates for installed KDE Plasma components.
///
/// Runs the full update pipeline: scan installed components, check for updates, select
/// which to apply, then download and install. Handles plasmashell restart based on
/// [`Config::restart`]. Components in [`Config::excluded_packages`] are always skipped.
///
/// With the `cli` feature enabled and [`Config::yes`] unset, shows an interactive
/// multi-select menu. Otherwise, all available updates are applied automatically.
///
/// # Errors
///
/// - [`UpdateError::Check`] — detection or fetch phase failed
/// - [`UpdateError::Other`] — one or more components failed to update
pub fn update(config: &Config) -> std::result::Result<UpdateResult, UpdateError> {
    crate::utils::validate_environment().map_err(UpdateError::Check)?;

    let api_client = ApiClient::new();
    let check_result =
        crate::utils::fetch_updates(&api_client, config).map_err(UpdateError::Check)?;

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

/// Result of performing updates.
///
/// Returned by [`update()`](crate::update). Tracks which components succeeded,
/// failed, or were skipped during the update run.
#[derive(Debug, Clone, Default)]
pub struct UpdateResult {
    pub succeeded: Vec<String>,
    pub failed: Vec<(String, String)>,
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

    /// Prints a formatted table of failed updates to stdout.
    #[cfg(feature = "cli")]
    pub fn print_error_table(&self) {
        crate::cli::output::print_error_table(self.clone());
    }

    /// Prints a one-line summary of the update outcome to stdout.
    #[cfg(feature = "cli")]
    pub fn print_summary(&self) {
        crate::cli::output::print_summary(self.clone());
    }
}

/// Error returned by [`update()`](crate::update) during the update phase.
#[derive(Debug)]
pub enum UpdateError {
    /// The check phase failed before updates could be attempted.
    Check(CheckError),
    /// An error occurred during the update process.
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Check(e) => write!(f, "{e}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Check(e) => Some(e),
            Self::Other(e) => Some(e.as_ref()),
        }
    }
}

impl From<Error> for UpdateError {
    fn from(e: Error) -> Self {
        Self::Other(Box::new(e))
    }
}

impl From<CheckError> for UpdateError {
    fn from(e: CheckError) -> Self {
        Self::Check(e)
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
pub fn install_update(update: &AvailableUpdate) -> Result<()> {
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
