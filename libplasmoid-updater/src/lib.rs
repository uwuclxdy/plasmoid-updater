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

//! Library for updating KDE Plasma 6 components from the KDE Store.
//!
//! The core entry points ([`check`], [`update`], [`install`]) never print,
//! prompt, read stdin or restart plasmashell; progress leaves only through a
//! [`Reporter`], and the opt-in [`cli`] feature provides the terminal pieces
//! ([`cli::ProgressReporter`], [`cli::select`], [`cli::print_failures`]) the
//! caller wires in explicitly.
//!
//! ```no_run
//! # #[cfg(feature = "cli")]
//! # fn main() -> Result<(), libplasmoid_updater::Error> {
//! use libplasmoid_updater::{Config, cli, update};
//!
//! let config = Config::new();
//! let reporter = cli::ProgressReporter::new();
//! let result = update(&config, &reporter)?;
//! if !result.failed.is_empty() {
//!     cli::print_failures(&result);
//! }
//! if result.needs_restart() {
//!     libplasmoid_updater::restart_plasmashell()?;
//! }
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "cli"))]
//! # fn main() {}
//! ```

#![warn(missing_docs)]

pub(crate) mod api;
pub(crate) mod checker;
pub(crate) mod config;
pub(crate) mod error;
pub(crate) mod installer;
pub(crate) mod paths;
pub(crate) mod registry;
pub(crate) mod reporter;
pub(crate) mod types;
pub(crate) mod utils;
pub(crate) mod version;

#[cfg(feature = "cli")]
pub mod cli;

use serde::Serialize;
use types::UpdateCheckResult;

pub use config::Config;
pub use error::Error;
pub use reporter::{Event, Reporter};
pub use types::{
    AvailableUpdate, ComponentType, Diagnostic, Failure, FailureKind, InstalledComponent,
};

/// A specialized `Result` type for libplasmoid-updater operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Checks for available updates to installed KDE Plasma components.
///
/// Scans the local filesystem for installed KDE components and queries the
/// store at [`Config::api_url`] for newer versions.
///
/// Components matching [`Config::excluded_packages`] or
/// [`Config::excluded_patterns`] leave `available_updates` and are listed in
/// [`CheckResult::skipped`] — this is the one place exclusions apply.
///
/// `available_updates` is empty when every checked component is current —
/// that is not an error. `diagnostics` can still list components that could
/// not be checked, so use [`CheckResult::has_updates`] for "is there anything
/// to install" and [`CheckResult::is_empty`] for "nothing found at all".
///
/// # Errors
///
/// - [`Error::UnsupportedOs`] — not running on Linux
/// - [`Error::NotKde`] — KDE Plasma not detected and
///   [`Config::skip_plasma_detection`] is unset
/// - [`Error::Network`] — a store request failed
/// - [`Error::RateLimited`] — the store asked the caller to back off
/// - [`Error::Api`] — the store returned a non-success status
/// - [`Error::Parse`] — a store response could not be parsed
/// - [`Error::Io`] — a filesystem scan failed
/// - [`Error::Config`] — the configuration is unusable
pub fn check(config: &Config) -> Result<CheckResult> {
    crate::utils::validate_environment(config.skip_plasma_detection)?;

    let api_client = api::ApiClient::new(config)?;
    let result = crate::utils::fetch_updates(&api_client, config)?;

    Ok(CheckResult::from_internal(crate::utils::apply_exclusions(
        result, config,
    )))
}

/// Result of checking for available updates.
///
/// Returned by [`check`]. Contains the full [`AvailableUpdate`] data
/// for each pending update, plus diagnostics for components that could not be checked.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Available updates found during the check.
    pub available_updates: Vec<AvailableUpdate>,
    /// Names of components excluded by the [`Config`]'s exclusion rules.
    /// Exclusions are applied once, here: excluded components never appear in
    /// `available_updates`.
    pub skipped: Vec<String>,
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
            skipped: result.skipped,
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

    /// Returns `true` if there are no updates, no diagnostics and no
    /// excluded components.
    pub fn is_empty(&self) -> bool {
        self.available_updates.is_empty() && self.diagnostics.is_empty() && self.skipped.is_empty()
    }
}

/// Downloads and installs every available update left after the check's
/// exclusions.
///
/// Runs the full pipeline: scan installed components, check the store, apply
/// [`Config`]'s exclusions (the same single filtering [`check`] does), then
/// download and install the rest on a thread pool. The names of excluded
/// components and of components the check could not resolve land in
/// [`UpdateResult::skipped`].
///
/// Progress and per-component outcomes are reported through `reporter`.
/// This function never prompts and never restarts plasmashell; call
/// [`restart_plasmashell`] when [`UpdateResult::needs_restart`] says so.
///
/// # Errors
///
/// Returns an [`Error`] when the run cannot start or finish:
/// - [`Error::AlreadyRunning`] — another run holds the update lock
/// - [`Error::MissingTool`] — `bsdtar` is missing, needed to extract archives
/// - the environment and store errors [`check`] documents
///
/// A component that fails to download, extract or install is not an `Error` —
/// it becomes a [`Failure`] row in [`UpdateResult::failed`] and the remaining
/// components are still attempted.
pub fn update(config: &Config, reporter: &dyn Reporter) -> Result<UpdateResult> {
    let _lock = installer::UpdateLock::acquire()?;
    crate::utils::validate_environment(config.skip_plasma_detection)?;
    crate::utils::require_extractor()?;

    reporter.event(&Event::Checking);

    let api_client = api::ApiClient::new(config)?;
    let check_result =
        crate::utils::apply_exclusions(crate::utils::fetch_updates(&api_client, config)?, config);

    let mut skipped = check_result.skipped;
    skipped.extend(
        check_result
            .unresolved
            .iter()
            .chain(check_result.check_failures.iter())
            .map(|d| d.name.clone()),
    );

    if check_result.updates.is_empty() {
        return Ok(UpdateResult {
            succeeded: Vec::new(),
            failed: Vec::new(),
            skipped,
        });
    }

    crate::utils::install_updates(
        &check_result.updates,
        &api_client,
        config,
        reporter,
        skipped,
    )
}

/// Downloads and installs a caller-chosen selection, such as the output of
/// [`check`] filtered by a prompt or a flag.
///
/// Installs exactly the selection it is given: [`Config`]'s exclusions do not
/// apply here. They are applied once, in [`check`] — a selection built from
/// [`CheckResult::available_updates`] already has the excluded components
/// removed. Passing an empty selection returns an empty result and changes
/// nothing. Like [`update`], this never prompts and never restarts
/// plasmashell.
///
/// # Errors
///
/// Returns an [`Error`] when the run cannot start or finish:
/// - [`Error::AlreadyRunning`] — another run holds the update lock
/// - [`Error::MissingTool`] — `bsdtar` is missing, needed to extract archives
/// - the environment and store errors [`check`] documents
///
/// Per-component problems become [`Failure`] rows in [`UpdateResult::failed`].
pub fn install(
    updates: &[AvailableUpdate],
    config: &Config,
    reporter: &dyn Reporter,
) -> Result<UpdateResult> {
    if updates.is_empty() {
        return Ok(UpdateResult::default());
    }

    let _lock = installer::UpdateLock::acquire()?;
    crate::utils::validate_environment(config.skip_plasma_detection)?;
    crate::utils::require_extractor()?;

    let api_client = api::ApiClient::new(config)?;

    crate::utils::install_updates(updates, &api_client, config, reporter, Vec::new())
}

/// The outcome of an [`update`] or [`install`] run.
///
/// [`Failure`] rows keep the full [`InstalledComponent`]; `skipped` lists the
/// names of excluded and unresolvable components.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateResult {
    /// Components that installed successfully and whose post-install version
    /// was verified.
    pub succeeded: Vec<InstalledComponent>,
    /// Components that failed, with the failed stage and the reason.
    pub failed: Vec<Failure>,
    /// Names of components not attempted: excluded by [`Config`], or
    /// unresolvable during the check.
    pub skipped: Vec<String>,
}

impl UpdateResult {
    /// Returns `true` when any touched component's type needs a plasmashell
    /// restart to pick up the new files: everything that installed
    /// successfully, plus failures whose files may still have changed
    /// ([`FailureKind::Unverified`], [`FailureKind::RestoreFailed`]).
    pub fn needs_restart(&self) -> bool {
        self.succeeded
            .iter()
            .chain(
                self.failed
                    .iter()
                    .filter(|f| {
                        matches!(f.kind, FailureKind::Unverified | FailureKind::RestoreFailed)
                    })
                    .map(|f| &f.component),
            )
            .any(|c| crate::installer::requires_restart(c.component_type))
    }
}

/// Returns all installed KDE Plasma components without making network requests.
///
/// Scans the filesystem and KNewStuff registry to discover locally installed components.
/// Useful for building custom UIs or auditing what is installed.
///
/// # Errors
///
/// Returns [`Error::Io`] if the filesystem scan fails.
pub fn get_installed(config: &Config) -> Result<Vec<InstalledComponent>> {
    checker::find_installed(config.system)
}

/// Restarts the plasmashell service via systemd. The core never calls this;
/// the caller decides when (and whether) to, typically when
/// [`UpdateResult::needs_restart`] returns `true`.
///
/// # Errors
///
/// Returns [`Error::MissingTool`] when `systemctl` is not on `PATH`, and
/// [`Error::Config`] naming the command when it cannot be spawned or exits
/// nonzero.
pub fn restart_plasmashell() -> Result<()> {
    installer::restart_plasmashell()
}
