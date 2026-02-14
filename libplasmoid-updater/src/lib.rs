//! # libplasmoid-updater
//!
//! A library for managing KDE Plasma components (plasmoids, themes, effects, etc.) from the KDE Store.
//!
//! ## Features
//!
//! - **Component Discovery**: Automatically discover installed KDE components
//! - **Update Detection**: Check for available updates from the KDE Store
//! - **Safe Installation**: Install updates with automatic backup and rollback
//! - **Registry Integration**: Maintain KNewStuff registry compatibility with KDE Discover
//! - **Parallel Processing**: Efficient parallel checking and updating
//!
//! ## Supported Component Types
//!
//! - Plasma Widgets (plasmoids)
//! - Wallpaper Plugins
//! - KWin Effects, Scripts, and Switchers
//! - Global Themes and Plasma Styles
//! - Color Schemes and Icon Themes
//! - Splash Screens and SDDM Themes
//! - Aurorae Decorations
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use libplasmoid_updater::{Config, run};
//!
//! # fn main() -> libplasmoid_updater::Result<()> {
//! let config = Config::new();
//! let summary = run(&config, false)?;
//! println!("Updated: {}, Failed: {}", summary.succeeded.len(), summary.failed.len());
//! # Ok(())
//! # }
//! ```
//!
//! ## Topgrade Integration
//!
//! This library is designed for easy integration into [topgrade](https://github.com/topgrade-rs/topgrade).
//! Here's the recommended pattern matching topgrade's conventions:
//!
//! ```rust,no_run
//! use libplasmoid_updater::{has_installed_components, run_default, Error};
//!
//! # struct ExecutionContext;
//! # impl ExecutionContext {
//! #     fn run_type(&self) -> RunType { RunType }
//! # }
//! # struct RunType;
//! # impl RunType {
//! #     fn dry(&self) -> bool { false }
//! # }
//! # #[derive(Debug)]
//! # enum TopgradeError { SkipStep(String), StepFailed }
//! # impl From<TopgradeError> for Box<dyn std::error::Error> {
//! #     fn from(_: TopgradeError) -> Self { todo!() }
//! # }
//! # fn print_separator(_: &str) {}
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let ctx = ExecutionContext;
//! pub fn run_plasmoid_updater(ctx: &ExecutionContext) -> Result<(), Box<dyn std::error::Error>> {
//!     // Check if any components are installed
//!     match has_installed_components(false) {
//!         Err(e) => {
//!             eprintln!("Error checking for installed components: {e:?}");
//!             return Err(TopgradeError::StepFailed.into());
//!         }
//!         Ok(false) => {
//!             return Err(TopgradeError::SkipStep("No KDE components installed".to_string()).into());
//!         }
//!         Ok(true) => {}
//!     }
//!
//!     print_separator("KDE Plasmoids");
//!
//!     // Topgrade handles dry run externally
//!     if ctx.run_type().dry() {
//!         println!("Dry running plasmoid-updater");
//!         return Ok(());
//!     }
//!
//!     // Run the updater
//!     match run_default(false) {
//!         Ok(summary) => {
//!             println!("Updated {} components", summary.succeeded.len());
//!             if !summary.failed.is_empty() {
//!                 println!("Failed to update {} components", summary.failed.len());
//!                 for (name, reason) in &summary.failed {
//!                     println!("  - {}: {}", name, reason);
//!                 }
//!                 return Err(TopgradeError::StepFailed.into());
//!             }
//!             Ok(())
//!         }
//!         Err(e) if e.is_skippable() => {
//!             Err(TopgradeError::SkipStep(e.to_string()).into())
//!         }
//!         Err(e) => {
//!             eprintln!("plasmoid-updater error: {e:?}");
//!             Err(TopgradeError::StepFailed.into())
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ### Error Handling
//!
//! The library provides helper methods for error categorization:
//! - [`Error::is_skippable()`] - Expected conditions (no updates, no components)
//! - [`Error::is_transient()`] - Temporary failures (network issues, rate limits)
//! - [`Error::is_fatal()`] - Permanent failures (permissions, IO errors)
//!
//! ### Configuration
//!
//! For custom exclusions or other settings, use the [`Config`] builder:
//!
//! ```rust,no_run
//! use libplasmoid_updater::{Config, run};
//!
//! # fn main() -> libplasmoid_updater::Result<()> {
//! let config = Config::new()
//!     .with_excluded_packages(vec!["problematic-widget".to_string()]);
//! let summary = run(&config, false)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Detailed Usage
//!
//! ```rust,no_run
//! use libplasmoid_updater::{ApiClient, Config, check_updates, update_components};
//!
//! # fn main() -> libplasmoid_updater::Result<()> {
//! let config = Config::new();
//! let api_client = ApiClient::new();
//!
//! // Check for available updates (user components)
//! let result = check_updates(&config, false, &api_client)?;
//! println!("Found {} updates", result.updates.len());
//!
//! // Update all components (excluding those in config.excluded_packages)
//! let summary = update_components(
//!     &result.updates, &config.excluded_packages, api_client.http_client(),
//! );
//! println!("Updated: {}, Failed: {}", summary.succeeded.len(), summary.failed.len());
//! # Ok(())
//! # }
//! ```
//!
//! ## Diagnostic Results
//!
//! ```rust,no_run
//! use libplasmoid_updater::{ApiClient, Config, check_updates};
//!
//! # fn main() -> libplasmoid_updater::Result<()> {
//! let config = Config::new();
//! let api_client = ApiClient::new();
//!
//! let result = check_updates(&config, false, &api_client)?;
//!
//! println!("Updates available: {}", result.updates.len());
//! println!("Unresolved components: {}", result.unresolved.len());
//! println!("Check failures: {}", result.check_failures.len());
//!
//! for diagnostic in &result.unresolved {
//!     println!("- {} ({})", diagnostic.name, diagnostic.reason);
//!     if let Some(version) = &diagnostic.installed_version {
//!         println!("  Installed version: {}", version);
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## System vs User Components
//!
//! The library supports both user-installed components (in ~/.local/share) and
//! system-wide components (in /usr/share). System operations require root privileges.
//!
//! ```rust,no_run
//! use libplasmoid_updater::{ApiClient, Config, check_updates};
//!
//! # fn main() -> libplasmoid_updater::Result<()> {
//! let config = Config::new();
//! let api_client = ApiClient::new();
//!
//! // Check system-wide components (requires sudo)
//! let result = check_updates(&config, true, &api_client)?;
//! # Ok(())
//! # }
//! ```

// SPDX-License-Identifier: MIT OR Apache-2.0
//
// This implementation is based on:
// - Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// - KDE Discover's KNewStuff backend (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+
//
// The update detection algorithm, KDE Store API interaction, and widget ID resolution
// approach are derived from Apdatifier's shell scripts. The KNewStuff registry format
// and installation process knowledge comes from KDE Discover's source code.

pub mod api;
pub mod backup;
pub mod checker;
pub mod config;
pub mod error;
pub mod installer;
pub(crate) mod paths;
pub mod registry;
pub mod types;
pub mod version;

pub use api::{ApiClient, ApiConfig, StatusCode, USER_AGENT};
pub use backup::{backup_component, restore_component};
pub use checker::find_installed;
pub use checker::{find_store_entry, select_download_url};
pub use config::Config;
pub use error::{Error, Result};
pub use installer::{
    any_requires_restart, restart_plasmashell, update_component, update_components,
};
pub use registry::{scan_registry_components, update_registry_after_install};
pub use types::{
    AvailableUpdate, ComponentDiagnostic, ComponentType, DownloadLink, InstalledComponent,
    KPluginInfo, PackageMetadata, StoreEntry, UpdateCheckResult, UpdateSummary,
};
pub use version::{compare as compare_versions, is_update_available};

/// Checks for available updates, returning full diagnostic results.
///
/// Scans installed KDE components (user or system), queries the KDE Store API,
/// and returns an [`UpdateCheckResult`] containing:
/// - `updates`: Components with available updates
/// - `unresolved`: Components that couldn't be matched to KDE Store entries
/// - `check_failures`: Components that matched but failed during update checking
///
/// # Arguments
///
/// * `config` - Configuration containing widgets-id table and excluded packages
/// * `system` - If `true`, checks system-wide components (requires root); if `false`, checks user components
/// * `api_client` - The API client to use for KDE Store queries
///
/// # Example
///
/// ```rust,no_run
/// use libplasmoid_updater::{ApiClient, Config, check_updates};
///
/// # fn main() -> libplasmoid_updater::Result<()> {
/// let config = Config::new();
/// let api_client = ApiClient::new();
/// let result = check_updates(&config, false, &api_client)?;
///
/// for update in &result.updates {
///     println!("{}: {} -> {}",
///         update.installed.name,
///         update.installed.version,
///         update.latest_version
///     );
/// }
/// # Ok(())
/// # }
/// ```
pub fn check_updates(
    config: &Config,
    system: bool,
    api_client: &ApiClient,
) -> Result<UpdateCheckResult> {
    checker::check(config, system, api_client)
}

/// Checks for updates and installs them in one step.
///
/// This is the primary entry point for automation tools like topgrade.
/// It combines [`check_updates`] and [`update_components`], respecting the
/// excluded packages list and dry_run setting from the configuration.
///
/// Returns [`Error::NoUpdatesAvailable`] if no updates are found.
///
/// # Arguments
///
/// * `config` - Configuration with excluded packages list and dry_run flag
/// * `system` - If `true`, operates on system-wide components; if `false`, user components
///
/// # Example
///
/// ```rust,no_run
/// use libplasmoid_updater::{Config, run};
///
/// # fn main() -> libplasmoid_updater::Result<()> {
/// let config = Config::new()
///     .with_excluded_packages(vec!["problematic-widget".to_string()]);
///
/// let summary = run(&config, false)?;
/// println!("Updated: {}", summary.succeeded.len());
/// println!("Failed: {}", summary.failed.len());
/// println!("Skipped: {}", summary.skipped.len());
/// # Ok(())
/// # }
/// ```
pub fn run(config: &Config, system: bool) -> Result<UpdateSummary> {
    let api_client = ApiClient::new();
    let result = check_updates(config, system, &api_client)?;

    if result.updates.is_empty() {
        return Err(Error::NoUpdatesAvailable);
    }

    if config.dry_run {
        let mut summary = UpdateSummary::default();
        for update in &result.updates {
            summary.add_skipped(update.installed.name.clone());
        }
        return Ok(summary);
    }

    Ok(update_components(
        &result.updates,
        &config.excluded_packages,
        api_client.http_client(),
    ))
}

/// Runs the updater with default configuration (topgrade integration).
///
/// This is a simplified entry point for automation tools that don't need
/// custom configuration. It uses default settings (no exclusions, no dry run).
///
/// Returns [`Error::NoUpdatesAvailable`] if no updates are found, which
/// can be converted to a `SkipStep` in topgrade using [`Error::is_skippable()`].
///
/// # Arguments
///
/// * `system` - If `true`, operates on system-wide components; if `false`, user components
///
/// # Example
///
/// ```rust,no_run
/// use libplasmoid_updater::run_default;
///
/// # fn main() -> libplasmoid_updater::Result<()> {
/// match run_default(false) {
///     Ok(summary) => {
///         println!("Updated: {}", summary.succeeded.len());
///         Ok(())
///     }
///     Err(e) if e.is_skippable() => {
///         println!("Skipping: {}", e);
///         Ok(())
///     }
///     Err(e) => Err(e),
/// }
/// # }
/// ```
pub fn run_default(system: bool) -> Result<UpdateSummary> {
    run(&Config::new(), system)
}

/// Checks if any KDE components are installed.
///
/// This is useful for early detection in automation tools - if no components
/// are found, the tool can skip the update step entirely.
///
/// # Arguments
///
/// * `system` - If `true`, checks system-wide components; if `false`, user components
///
/// # Example
///
/// ```rust,no_run
/// use libplasmoid_updater::has_installed_components;
///
/// # fn main() -> libplasmoid_updater::Result<()> {
/// if has_installed_components(false)? {
///     println!("Found installed components, proceeding with update check...");
/// } else {
///     println!("No installed components found, skipping.");
/// }
/// # Ok(())
/// # }
/// ```
pub fn has_installed_components(system: bool) -> Result<bool> {
    Ok(!find_installed(system)?.is_empty())
}

/// Lists all installed KDE components.
///
/// Scans the filesystem and KNewStuff registry to discover all installed components,
/// including their versions, types, and installation paths.
///
/// # Arguments
///
/// * `system` - If `true`, lists system-wide components; if `false`, lists user components
///
/// # Example
///
/// ```rust,no_run
/// use libplasmoid_updater::list_installed;
///
/// # fn main() -> libplasmoid_updater::Result<()> {
/// let components = list_installed(false)?;
///
/// for component in components {
///     println!("{} ({}): version {}",
///         component.name,
///         component.component_type,
///         component.version
///     );
/// }
/// # Ok(())
/// # }
/// ```
pub fn list_installed(system: bool) -> Result<Vec<InstalledComponent>> {
    find_installed(system)
}
