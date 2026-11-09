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
//! ([`cli::ProgressReporter`], [`cli::print_above`], [`cli::select`],
//! [`cli::print_failures`], [`cli::update_summary`]) the caller wires in
//! explicitly.
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
#[cfg(test)]
pub(crate) mod testutil;
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
    AvailableUpdate, AvailableUpdateBuilder, ComponentType, Diagnostic, Failure, FailureKind,
    InstalledComponent,
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
/// [`CheckResult::skipped`]; [`update`] applies the same rules to its own run.
///
/// `available_updates` is empty when every checked component is current —
/// that is not an error. `diagnostics` can still list components that could
/// not be checked, so use [`CheckResult::has_updates`] for "is there anything
/// to install" and [`CheckResult::is_empty`] for "nothing found at all".
///
/// Once the scan has counted the installed components, one
/// [`Event::Checking`] carrying that count leaves through `reporter`; one
/// [`Event::Checked`] closes the check part on every path, a store error or a
/// panic unwinding out of the check included, so a failed check never leaves
/// the checking line live.
///
/// # Errors
///
/// - [`Error::UnsupportedOs`] — not running on Linux
/// - [`Error::NotKde`] — KDE Plasma not detected and
///   [`Config::skip_plasma_detection`] is unset
/// - [`Error::Network`] — a store request failed
/// - [`Error::RateLimited`] — the store asked the caller to back off
/// - [`Error::Api`] — the store returned a non-success status
/// - [`Error::Parse`] — a store response could not be parsed (a KNewStuff
///   registry file that cannot be parsed is logged and skipped instead)
/// - [`Error::Io`] — a filesystem scan failed
/// - [`Error::Config`] — the configuration is unusable
pub fn check(config: &Config, reporter: &dyn Reporter) -> Result<CheckResult> {
    crate::utils::validate_environment(config.skip_plasma_detection)?;

    let api_client = api::ApiClient::new(config)?;
    let result = crate::utils::fetch_updates(&api_client, config, reporter)?;

    Ok(CheckResult::from_internal(crate::utils::apply_exclusions(
        result, config,
    )))
}

/// Result of checking for available updates.
///
/// Returned by [`check`]. Contains the full [`AvailableUpdate`] data
/// for each pending update, plus diagnostics for components that could not be checked.
///
/// The struct is `#[non_exhaustive]`, so a later field ships in a minor
/// release. [`check`] produces it; a host builds a run of its own from
/// [`CheckResult::default`] and field assignment, never a struct literal from
/// outside this crate:
///
/// ```
/// use libplasmoid_updater::CheckResult;
///
/// let mut result = CheckResult::default();
/// result.skipped.push("excluded.widget".to_string());
/// assert_eq!(result.skipped, vec!["excluded.widget".to_string()]);
/// ```
///
/// ```compile_fail
/// use libplasmoid_updater::CheckResult;
///
/// let _ = CheckResult {
///     available_updates: Vec::new(),
///     skipped: Vec::new(),
///     unmatched: Vec::new(),
///     diagnostics: Vec::new(),
///     unavailable: Vec::new(),
/// };
/// ```
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct CheckResult {
    /// Available updates found during the check.
    pub available_updates: Vec<AvailableUpdate>,
    /// Display names of components the run did not attempt: excluded by the
    /// [`Config`]'s exclusion rules, and — in a `--system` run — components
    /// with no matching store entry or whose store entry is of a different
    /// component type.
    ///
    /// An excluded component never appears in `available_updates`. Display
    /// names are not unique, so a name here does not say which component was
    /// skipped: [`unmatched`](Self::unmatched) holds the components skipped
    /// for want of a store entry, and [`Config::excludes`] tells the excluded
    /// ones.
    pub skipped: Vec<String>,
    /// Components the run skipped because no store entry of their type
    /// matches them: no entry matches, or the matching entry is another
    /// component type. Only a `--system` run skips a component for this; a
    /// user run reports it in `diagnostics` instead.
    ///
    /// Each one's display name is also in `skipped`. An excluded component is
    /// never listed here.
    pub unmatched: Vec<InstalledComponent>,
    /// Components that could not be checked, with the reason for each failure.
    /// Each [`Diagnostic::path`] names its component.
    pub diagnostics: Vec<Diagnostic>,
    /// Components whose store entry offers no download link for its version.
    ///
    /// Each is a [`FailureKind::NoMatchingDownload`] row a caller reports the
    /// way [`update`] does: the store answered, this run just could not
    /// install it. Each component is also listed in `diagnostics`, so a
    /// `check` still prints why.
    pub unavailable: Vec<Failure>,
}

impl CheckResult {
    pub(crate) fn from_internal(result: UpdateCheckResult) -> Self {
        let mut diagnostics: Vec<Diagnostic> = result
            .unresolved
            .into_iter()
            .chain(result.check_failures)
            .collect();
        let mut unavailable = Vec::new();
        for entry in result.no_matching_download {
            diagnostics.push(entry.diagnostic.clone());
            unavailable.push(entry.into_failure());
        }

        Self {
            available_updates: result.updates,
            skipped: result.skipped,
            unmatched: result.unmatched,
            diagnostics,
            unavailable,
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

    /// The diagnostics carrying the no-download reason, one per
    /// [`Self::unavailable`] row.
    ///
    /// Each is one component whose store entry offered no download link for
    /// its version. A caller that reports [`Self::unavailable`] as failures
    /// uses this to keep those components out of the plain diagnostic lines
    /// and out of `skipped`.
    pub fn unavailable_diagnostics(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.reason == types::NO_DOWNLOAD_URL_REASON)
    }
}

/// Downloads and installs every available update left after the check's
/// exclusions.
///
/// Runs the full pipeline: scan installed components, check the store, apply
/// [`Config`]'s exclusions (the same single filtering [`check`] does), then
/// download and install the rest on a thread pool. The names of excluded
/// components and of components the check could not resolve land in
/// [`UpdateResult::skipped`]; a component whose entry offers no download link
/// for its version is a [`FailureKind::NoMatchingDownload`] row in
/// [`UpdateResult::failed`].
///
/// One [`Event::Checking`] carrying the scanned component count is the first
/// event, and one [`Event::Checked`] closes the check part on every path;
/// progress and per-component outcomes follow through `reporter`. This function never
/// prompts and never restarts plasmashell; call [`restart_plasmashell`] when
/// [`UpdateResult::needs_restart`] says so.
///
/// # Errors
///
/// Returns an [`Error`] when the run cannot start or finish:
/// - [`Error::AlreadyRunning`] — another run holds the update lock
/// - [`Error::MissingTool`] — `bsdtar` is missing, needed to extract archives
/// - [`Error::Io`] naming the lock file — a sudo run could not take the
///   invoking user's ids to create it in that user's cache
/// - the environment and store errors [`check`] documents
///
/// A component that fails to download, extract or install is not an `Error` —
/// it becomes a [`Failure`] row in [`UpdateResult::failed`] and the remaining
/// components are still attempted.
pub fn update(config: &Config, reporter: &dyn Reporter) -> Result<UpdateResult> {
    let _lock = installer::UpdateLock::acquire()?;
    crate::utils::validate_environment(config.skip_plasma_detection)?;
    crate::utils::require_extractor()?;
    repair_after_interrupted_run(config);

    let api_client = api::ApiClient::new(config)?;
    let fetched = crate::utils::fetch_updates(&api_client, config, reporter)?;
    let mut check_result = crate::utils::apply_exclusions(fetched, config);

    let mut skipped = check_result.skipped;
    skipped.extend(
        check_result
            .unresolved
            .iter()
            .chain(check_result.check_failures.iter())
            .map(|d| d.name.clone()),
    );

    // A component whose entry offers no download link for its version is a
    // failure, not a skip: the store answered, this run just could not install
    // it. It carries the component the check resolved.
    let failed: Vec<Failure> = check_result
        .no_matching_download
        .drain(..)
        .map(types::NoMatchingDownload::into_failure)
        .collect();

    if check_result.updates.is_empty() {
        return Ok(UpdateResult {
            succeeded: Vec::new(),
            failed,
            skipped,
        });
    }

    let mut result = crate::utils::install_updates(
        &check_result.updates,
        &api_client,
        config,
        reporter,
        skipped,
    )?;
    result.failed.extend(failed);
    Ok(result)
}

/// Downloads and installs a caller-chosen selection, such as the output of
/// [`check`] filtered by a prompt or a flag.
///
/// Installs exactly the selection it is given: [`Config`]'s exclusions do not
/// apply here. [`check`] and [`update`] apply them to their own results — a
/// selection built from [`CheckResult::available_updates`] already has the
/// excluded components removed. An empty selection installs nothing, but still
/// takes the run lock and repairs what a killed run left stranded, so an
/// interrupted install is recovered even when this run has nothing to install.
/// When another run already holds the lock, an empty selection returns an
/// empty result without repairing; a non-empty selection still fails. Like
/// [`update`], this never prompts and never restarts plasmashell.
///
/// A selected update whose [`download_url`](AvailableUpdate::download_url) is
/// empty is a [`FailureKind::NoMatchingDownload`] row: it is refused before any
/// backup or download starts, and the rest of the selection continues.
///
/// # Errors
///
/// Returns an [`Error`] when the run cannot start or finish:
/// - [`Error::AlreadyRunning`] — another run holds the update lock, and the
///   selection is not empty
/// - [`Error::MissingTool`] — `bsdtar` is missing, needed to extract archives
///   when the selection is not empty
/// - [`Error::Io`] naming the lock file — a sudo run could not take the
///   invoking user's ids to create it in that user's cache
/// - the environment and store errors [`check`] documents
///
/// Per-component problems become [`Failure`] rows in [`UpdateResult::failed`].
pub fn install(
    updates: &[AvailableUpdate],
    config: &Config,
    reporter: &dyn Reporter,
) -> Result<UpdateResult> {
    let _lock = match installer::UpdateLock::acquire() {
        Ok(lock) => lock,
        Err(Error::AlreadyRunning) if updates.is_empty() => {
            log::info!(
                target: "lock",
                "another run holds the update lock; this run skips the interrupted-install repair"
            );
            return Ok(UpdateResult::default());
        }
        Err(error) => return Err(error),
    };
    crate::utils::validate_environment(config.skip_plasma_detection)?;
    repair_after_interrupted_run(config);

    if updates.is_empty() {
        return Ok(UpdateResult::default());
    }

    crate::utils::require_extractor()?;

    let api_client = api::ApiClient::new(config)?;

    crate::utils::install_updates(updates, &api_client, config, reporter, Vec::new())
}

/// Clears what a run killed mid-install left behind, before this run scans or
/// installs anything.
///
/// The caller holds the run lock, which is what makes every one of these
/// leftovers a dead run's: component trees stranded in hidden siblings by a
/// swap that never finished, and download directories in the temp dir.
fn repair_after_interrupted_run(config: &Config) {
    installer::recover_interrupted_installs(config.system);
    installer::remove_stale_temp_dirs();
}

/// The outcome of an [`update`] or [`install`] run.
///
/// [`Failure`] rows keep the full [`InstalledComponent`]; `skipped` lists the
/// names of excluded and unresolvable components.
///
/// The struct is `#[non_exhaustive]`, so a later field ships in a minor
/// release: [`update`] and [`install`] produce it, and a host builds a run of
/// its own from [`UpdateResult::default`] and field assignment, never a struct
/// literal from outside this crate.
///
/// ```
/// use libplasmoid_updater::UpdateResult;
///
/// let mut result = UpdateResult::default();
/// result.skipped.push("excluded.widget".to_string());
/// assert_eq!(result.skipped, vec!["excluded.widget".to_string()]);
/// ```
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct UpdateResult {
    /// Components that installed successfully and whose post-install version
    /// was verified, each carrying that installed version and the store's
    /// release date for it (the registry may not record that date, so a later
    /// [`check`] can read an older or empty one).
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
    /// ([`FailureKind::Unverified`], [`FailureKind::Registry`],
    /// [`FailureKind::RestoreFailed`]).
    pub fn needs_restart(&self) -> bool {
        self.succeeded
            .iter()
            .chain(
                self.failed
                    .iter()
                    .filter(|f| {
                        matches!(
                            f.kind,
                            FailureKind::Unverified
                                | FailureKind::Registry
                                | FailureKind::RestoreFailed
                        )
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
/// A registry file that does not parse is logged at error level and skipped, so
/// a broken registry never hides the components discovered from anywhere else.
///
/// # Errors
///
/// Returns [`Error::Io`] if the filesystem scan fails.
pub fn get_installed(config: &Config) -> Result<Vec<InstalledComponent>> {
    checker::find_installed(config.system)
}

/// Returns whether a killed earlier run left anything in the component
/// directories that [`install`]'s repair settles: any hidden `-new`/`-old`
/// sibling it finds — a component stranded in `-old`, both copies of an
/// interrupted swap, a dead copy beside a live component, or a lone `-new`
/// copy of an install killed mid-copy.
///
/// It scans the directories [`install`]'s repair scans, user-local ones or,
/// under [`Config::system`], system-wide ones, and reads the disk only: it
/// never prompts and needs no privilege. A caller that elevates for a
/// system-wide run asks it before the first [`install`], since only that
/// repair can need privilege when the selection is empty. Download
/// directories a killed run left in the temp dir do not count: removing them
/// needs no privilege.
///
/// While another run holds the update lock, an empty [`install`] returns
/// without repairing, so a held lock answers `false`.
///
/// # Errors
///
/// The environment errors [`install`] fails on before its repair:
/// - [`Error::UnsupportedOs`] — not running on Linux
/// - [`Error::NotKde`] — KDE Plasma not detected and
///   [`Config::skip_plasma_detection`] is unset
///
/// # Examples
///
/// ```no_run
/// # fn main() -> Result<(), libplasmoid_updater::Error> {
/// use libplasmoid_updater::{Config, install, needs_repair};
///
/// let config = Config::new().with_system(true);
/// if needs_repair(&config)? {
///     // elevate here, before the repair needs it
/// }
/// install(&[], &config, &())?;
/// # Ok(())
/// # }
/// ```
pub fn needs_repair(config: &Config) -> Result<bool> {
    crate::utils::validate_environment(config.skip_plasma_detection)?;
    Ok(!installer::run_lock_held() && installer::has_interrupted_installs(config.system))
}

/// Restarts the systemd user unit whose main process owns
/// `org.kde.plasmashell` on the session bus, then waits for a new plasmashell
/// to take that name. With no plasmashell running it starts
/// `plasma-plasmashell.service`. The core never calls this; the caller decides
/// when (and whether) to, typically when [`UpdateResult::needs_restart`]
/// returns `true`.
///
/// # Errors
///
/// Returns [`Error::Restart`] when plasmashell runs in a unit that is not a
/// service whose main process it is, when it belongs to no systemd user unit
/// or no systemd user manager is on the session bus, when the old plasmashell
/// still owns the bus name after the restart, or when no new plasmashell owns
/// it within ten seconds of the restart. Returns [`Error::MissingTool`] when
/// `busctl` or `systemctl` is not on `PATH`, [`Error::Config`] naming the
/// command when it cannot be spawned, [`Error::Command`] naming the command,
/// its status and the reason it printed when it exits nonzero, and
/// [`Error::Parse`] when `busctl` prints output this library cannot parse.
pub fn restart_plasmashell() -> Result<()> {
    installer::restart_plasmashell()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// `from_internal` carries a populated `unmatched` into the public
    /// `CheckResult`, so a `--system` run's store-entry skips reach callers
    /// as components, not only as `skipped` names.
    #[test]
    fn from_internal_carries_unmatched_into_the_check_result() {
        let component = InstalledComponent::new(
            "Breeze Dark",
            "BreezeDark.colors",
            ComponentType::ColorScheme,
            "/usr/share/color-schemes/BreezeDark.colors",
        );
        let mut internal = UpdateCheckResult::default();
        internal.unmatched.push(component.clone());

        let result = CheckResult::from_internal(internal);

        assert_eq!(
            result.unmatched.len(),
            1,
            "unmatched did not carry over: {result:?}"
        );
        let carried = &result.unmatched[0];
        assert_eq!(
            carried.path, component.path,
            "unmatched carried the wrong component"
        );
        assert_eq!(carried.component_type, component.component_type);
    }

    const LOCK_CHILD: &str = "tests::an_empty_install_defers_the_repair_to_a_running_lock_holder";
    const LOCK_ROLE: &str = "PU_TEST_EMPTY_INSTALL_ROLE";
    const LOCK_ROOT: &str = "PU_TEST_EMPTY_INSTALL_ROOT";

    fn lock_child(root: &Path, role: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", LOCK_CHILD, "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root.join("tmp"))
            .env(LOCK_ROLE, role)
            .env(LOCK_ROOT, root);
        command
    }

    fn wait_for(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// An empty `install()` under another run's lock has nothing to take: it
    /// returns the empty result and leaves the interrupted-install repair to
    /// the run that holds the lock, instead of failing the caller.
    #[test]
    fn an_empty_install_defers_the_repair_to_a_running_lock_holder() {
        let Some(role) = std::env::var_os(LOCK_ROLE) else {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            for name in ["home", "data", "config", "cache", "runtime", "tmp"] {
                std::fs::create_dir_all(root.join(name)).unwrap();
            }

            let mut holder = lock_child(&root, "hold").spawn().unwrap();
            wait_for(&root.join("held"));
            let body = lock_child(&root, "body").output().unwrap();
            std::fs::write(root.join("release"), b"ok").unwrap();
            let status = holder.wait().unwrap();

            assert!(
                body.status.success() && root.join("body-done").exists(),
                "the empty install failed under a held lock ({}; body completed: {})\n--- stderr\n{}",
                body.status,
                root.join("body-done").exists(),
                String::from_utf8_lossy(&body.stderr),
            );
            assert!(status.success(), "the holding child failed");
            return;
        };

        let root = std::path::PathBuf::from(std::env::var(LOCK_ROOT).unwrap());
        match role.to_str().unwrap() {
            "hold" => {
                let _lock =
                    installer::UpdateLock::acquire().expect("the holding child could not lock");
                std::fs::write(root.join("held"), b"ok").unwrap();
                wait_for(&root.join("release"));
            }
            "body" => {
                testutil::logs::capture_logs();
                let config = Config::new()
                    .with_skip_plasma_detection(true)
                    .with_inhibit_idle(false);

                let result = install(&[], &config, &())
                    .expect("an empty install under a held lock must defer, not fail");

                assert!(
                    result.succeeded.is_empty() && result.failed.is_empty(),
                    "{result:?}"
                );
                let logged = testutil::logs::captured_logs();
                assert!(
                    logged.iter().any(|line| line.starts_with("lock: ")
                        && line.contains("another run holds the update lock")),
                    "the deferral was not logged: {logged:?}"
                );
                std::fs::write(root.join("body-done"), b"ok").unwrap();
            }
            other => panic!("unknown role {other}"),
        }
    }

    #[test]
    fn a_registry_failure_on_a_plugin_type_counts_toward_needs_restart() {
        let result = UpdateResult {
            succeeded: Vec::new(),
            failed: vec![Failure {
                component: InstalledComponent {
                    name: "widget".to_string(),
                    directory_name: "widget".to_string(),
                    version: "1.0".to_string(),
                    component_type: ComponentType::PlasmaWidget,
                    path: PathBuf::from("/usr/share/plasma/plasmoids/widget"),
                    is_system: true,
                    release_date: String::new(),
                },
                kind: FailureKind::Registry,
                message: "the registry could not be rewritten".to_string(),
            }],
            skipped: Vec::new(),
        };

        assert!(result.needs_restart(), "{result:?}");
    }
}
