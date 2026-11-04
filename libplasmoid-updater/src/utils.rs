// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{
    Config, Error, Event, Failure, FailureKind, Reporter, UpdateResult,
    api::ApiClient,
    checker::{check_with_components, find_installed},
    installer::{self, ComponentFailure, InstallOutcome},
    types::{AvailableUpdate, InstalledComponent, UpdateCheckResult},
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

pub(crate) fn validate_environment(skip_plasma_detection: bool) -> crate::Result<()> {
    if cfg!(not(target_os = "linux")) {
        return Err(Error::UnsupportedOs);
    }
    let plasma_found = skip_plasma_detection || crate::paths::is_kde();
    if !plasma_found {
        return Err(Error::NotKde);
    }
    Ok(())
}

/// Fails when `bsdtar` is not on `PATH`. Only installs extract archives, so
/// only [`update`](crate::update) and [`install`](crate::install) need it.
pub(crate) fn require_extractor() -> crate::Result<()> {
    check_dependency("bsdtar")
}

fn check_dependency(name: &'static str) -> crate::Result<()> {
    use std::process::Command;
    match Command::new("which").arg(name).output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err(Error::MissingTool { tool: name }),
    }
}

pub(crate) fn fetch_updates(
    api_client: &ApiClient,
    config: &Config,
    reporter: &dyn Reporter,
) -> crate::Result<UpdateCheckResult> {
    let components = find_installed(config.system)?;
    reporter.event(&Event::Checking {
        components: components.len(),
    });
    // Closes the check part on every path out of this function, a panic
    // unwinding out of the store check included.
    let _checked = CheckedOnDrop(reporter);
    check_with_components(config, api_client, components)
}

/// Emits [`Event::Checked`] once when dropped, so a check that sent
/// [`Event::Checking`] always sends its closing event, a panic included.
struct CheckedOnDrop<'a>(&'a dyn Reporter);

impl Drop for CheckedOnDrop<'_> {
    fn drop(&mut self) {
        self.0.event(&Event::Checked);
    }
}

/// Decides whether a candidate update is excluded from selection.
///
/// Combines an exact-match list (matched against directory and display name)
/// with a set of regex patterns compiled once up front. Mirrors Apdatifier's
/// two rule types: `name` (exact) and `regex`.
pub(crate) struct ExcludeMatcher<'a> {
    exact: &'a [String],
    patterns: Option<regex::RegexSet>,
}

impl<'a> ExcludeMatcher<'a> {
    /// Builds a matcher, compiling `patterns` once. Invalid patterns are
    /// dropped with a warning rather than aborting the run.
    pub(crate) fn new(exact: &'a [String], patterns: &'a [String]) -> ExcludeMatcher<'a> {
        Self {
            exact,
            patterns: compile_exclude_patterns(patterns, InvalidPattern::Warn),
        }
    }

    /// Builds a matcher like [`new`](Self::new) but drops invalid patterns
    /// silently: a run's own matcher already warned about them, and a caller
    /// asking about one component after that run must not warn again.
    pub(crate) fn quiet(exact: &'a [String], patterns: &'a [String]) -> ExcludeMatcher<'a> {
        Self {
            exact,
            patterns: compile_exclude_patterns(patterns, InvalidPattern::Ignore),
        }
    }

    pub(crate) fn is_excluded(&self, update: &AvailableUpdate) -> bool {
        self.is_excluded_component(&update.installed)
    }

    /// The same rules against a bare component, for a check result that
    /// carries no [`AvailableUpdate`] for it.
    pub(crate) fn is_excluded_component(&self, component: &InstalledComponent) -> bool {
        let dir = &component.directory_name;
        let name = &component.name;

        if self.exact.iter().any(|e| e == dir || e == name) {
            return true;
        }

        match &self.patterns {
            Some(set) => set.is_match(dir) || set.is_match(name),
            None => false,
        }
    }
}

/// What compiling an exclusion set does with a pattern that does not parse.
#[derive(Clone, Copy)]
enum InvalidPattern {
    Warn,
    Ignore,
}

/// Compiles exclusion patterns into a single [`regex::RegexSet`].
///
/// Each pattern is validated individually so a single bad pattern only drops
/// itself (with a warning under [`InvalidPattern::Warn`]) instead of discarding
/// the whole set. Returns `None` when no usable pattern remains, letting the
/// matcher skip regex work entirely.
fn compile_exclude_patterns(
    patterns: &[String],
    invalid: InvalidPattern,
) -> Option<regex::RegexSet> {
    let valid: Vec<&str> = patterns
        .iter()
        .filter(|p| match regex::Regex::new(p) {
            Ok(_) => true,
            Err(e) => {
                match invalid {
                    InvalidPattern::Warn => {
                        log::warn!(target: "config", "ignoring invalid exclude pattern {p:?}: {e}");
                    }
                    InvalidPattern::Ignore => {}
                }
                false
            }
        })
        .map(String::as_str)
        .collect();

    if valid.is_empty() {
        return None;
    }

    regex::RegexSet::new(&valid).ok()
}

/// Applies `config`'s exclusion rules to a check result: excluded components
/// leave `updates` and their names land in `skipped`.
///
/// [`check`](crate::check) and [`update`](crate::update) each call it on their
/// own result, and every later stage works on the filtered list;
/// [`install`](crate::install) applies no exclusions.
///
/// A component with no matching download is filtered here too: an excluded
/// component the store could not serve stays a skip, never a failure row.
pub(crate) fn apply_exclusions(
    mut result: UpdateCheckResult,
    config: &Config,
) -> UpdateCheckResult {
    let matcher = ExcludeMatcher::new(&config.excluded_packages, &config.excluded_patterns);
    let mut selected = Vec::new();
    for update in result.updates.drain(..) {
        if matcher.is_excluded(&update) {
            result.skipped.push(update.installed.name);
        } else {
            selected.push(update);
        }
    }
    result.updates = selected;

    let mut unavailable = Vec::new();
    for entry in result.no_matching_download.drain(..) {
        if matcher.is_excluded_component(&entry.component) {
            result.skipped.push(entry.component.name);
        } else {
            unavailable.push(entry);
        }
    }
    result.no_matching_download = unavailable;
    result
}

/// Installs `updates` (already exclusion-filtered) on a rayon pool, emitting
/// progress through `reporter` and collecting per-component outcomes into an
/// [`UpdateResult`].
pub(crate) fn install_updates(
    updates: &[AvailableUpdate],
    api_client: &ApiClient,
    config: &Config,
    reporter: &dyn Reporter,
    skipped: Vec<String>,
) -> crate::Result<UpdateResult> {
    let _inhibit = if config.inhibit_idle {
        installer::InhibitGuard::acquire()
    } else {
        installer::InhibitGuard::None
    };

    let mut pool_builder = rayon::ThreadPoolBuilder::new();
    if let Some(thread_count) = config.threads {
        pool_builder = pool_builder.num_threads(thread_count);
    }
    let pool = pool_builder
        .build()
        .map_err(|e| Error::Config(format!("failed to build install thread pool: {e}")))?;

    let backups = installer::BackupLedger::default();
    let outcomes: Vec<(&AvailableUpdate, Result<InstallOutcome, ComponentFailure>)> =
        pool.install(|| {
            updates
                .par_iter()
                .map(|update| {
                    let outcome = installer::update_component(
                        update,
                        api_client.http_client(),
                        reporter,
                        &backups,
                    );
                    report_outcome(&update.installed, &outcome, reporter);
                    (update, outcome)
                })
                .collect()
        });
    // Retention runs once every install has finished, so no component's backup
    // can be pruned by a later component of its own type while it is still
    // being installed.
    installer::prune_backups(&backups);

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for (update, outcome) in outcomes {
        let component = update.installed.clone();
        match outcome {
            // Verified means the version on disk is the expected one; the
            // release date is the store's for that version, in the registry's
            // date-only form.
            Ok(outcome) if outcome.verified => succeeded.push(InstalledComponent {
                version: outcome.expected_version,
                release_date: crate::registry::extract_date_from_iso(&update.release_date),
                ..component
            }),
            Ok(outcome) => failed.push(Failure {
                component,
                kind: FailureKind::Unverified,
                message: unverified_message(&outcome),
            }),
            Err(e) => failed.push(e.into_failure(component)),
        }
    }

    Ok(UpdateResult {
        succeeded,
        failed,
        skipped,
    })
}

/// Emits the per-component [`Event::Installed`] / [`Event::Failed`] for a
/// finished component. Called inside the install pool, as each component
/// finishes, so progress is not held back by slower siblings.
fn report_outcome(
    component: &InstalledComponent,
    outcome: &Result<InstallOutcome, ComponentFailure>,
    reporter: &dyn Reporter,
) {
    let id = component.event_id();
    match outcome {
        Ok(outcome) if outcome.verified => {
            reporter.event(&Event::Installed {
                name: component.name.clone(),
                id,
                version: outcome.expected_version.clone(),
            });
        }
        Ok(outcome) => {
            reporter.event(&Event::Failed {
                name: component.name.clone(),
                id,
                message: unverified_message(outcome),
            });
        }
        Err(e) => {
            log::debug!(target: "update", "failed for {}: {}", component.name, e.message);
            reporter.event(&Event::Failed {
                name: component.name.clone(),
                id,
                message: e.message.clone(),
            });
        }
    }
}

fn unverified_message(outcome: &InstallOutcome) -> String {
    let actual = outcome.actual_version.as_deref().unwrap_or("(unreadable)");
    format!(
        "installed, but expected version {}, found {}",
        outcome.expected_version, actual
    )
}

#[cfg(test)]
mod tests {
    use crate::testutil::logs::captured_logs_while;

    use super::{ExcludeMatcher, InvalidPattern, compile_exclude_patterns};

    #[test]
    fn compile_keeps_valid_patterns_and_matches_both_substrings_and_anchors() {
        let set = compile_exclude_patterns(
            &[r"^org\.kde\.".to_string(), "weather".to_string()],
            InvalidPattern::Warn,
        )
        .expect("two valid patterns should compile");

        assert!(set.is_match("org.kde.plasma.systemmonitor"));
        assert!(set.is_match("my-weather-widget"));
        assert!(!set.is_match("com.example.clock"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn compile_drops_only_the_invalid_pattern() {
        let set = compile_exclude_patterns(
            &[
                "valid".to_string(),
                "(".to_string(), // unbalanced group — invalid
            ],
            InvalidPattern::Warn,
        )
        .expect("the one valid pattern should survive");

        assert_eq!(set.len(), 1);
        assert!(set.is_match("a valid name"));
    }

    #[test]
    fn compile_returns_none_when_empty_or_all_invalid() {
        assert!(compile_exclude_patterns(&[], InvalidPattern::Warn).is_none());
        assert!(compile_exclude_patterns(&["(".to_string()], InvalidPattern::Warn).is_none());
    }

    /// Invalid patterns no other test compiles, one per test: the capture is
    /// process-wide, so a pattern two concurrent tests share lets one test's
    /// warning land in the other's window.
    const QUIET_INVALID_PATTERN: &str = "(quiet-unclosed";
    const RUN_INVALID_PATTERN: &str = "(run-unclosed";

    fn invalid_pattern_warnings<'a>(lines: &'a [String], pattern: &str) -> Vec<&'a str> {
        let warning = format!("ignoring invalid exclude pattern {pattern:?}");
        lines
            .iter()
            .filter(|line| line.contains(&warning))
            .map(String::as_str)
            .collect()
    }

    /// An invalid pattern is dropped with a warning on the run's matcher, but
    /// asking `Config::excludes` after that run repeats no warning.
    #[test]
    fn excludes_logs_no_warning_for_an_invalid_pattern() {
        let config =
            crate::Config::new().with_excluded_patterns(vec![QUIET_INVALID_PATTERN.to_string()]);
        let component = crate::InstalledComponent::new(
            "Weather Widget",
            "org.example.weather",
            crate::ComponentType::PlasmaWidget,
            "/plasmoids/org.example.weather",
        );

        let mut excluded = None;
        let logged = captured_logs_while(|| {
            excluded = Some(config.excludes(&component));
        });

        assert_eq!(
            excluded,
            Some(false),
            "an invalid pattern excluded the component"
        );
        let warnings = invalid_pattern_warnings(&logged, QUIET_INVALID_PATTERN);
        assert!(
            warnings.is_empty(),
            "asking whether a component is excluded warned again: {warnings:?}"
        );
    }

    #[test]
    fn the_run_matcher_logs_an_invalid_pattern_once() {
        let patterns = vec![RUN_INVALID_PATTERN.to_string()];
        let logged = captured_logs_while(|| {
            let _matcher = ExcludeMatcher::new(&[], &patterns);
        });

        let warnings = invalid_pattern_warnings(&logged, RUN_INVALID_PATTERN);
        assert_eq!(
            warnings.len(),
            1,
            "the run's matcher did not warn exactly once for the invalid pattern: {warnings:?}"
        );
    }

    #[test]
    fn an_excluded_component_with_no_matching_download_is_skipped_not_failed() {
        use std::path::PathBuf;

        use crate::types::{
            ComponentType, Diagnostic, InstalledComponent, NoMatchingDownload, UpdateCheckResult,
        };

        fn no_match(name: &str) -> NoMatchingDownload {
            NoMatchingDownload {
                component: InstalledComponent {
                    name: name.to_string(),
                    directory_name: name.to_string(),
                    version: "1.0".to_string(),
                    component_type: ComponentType::PlasmaWidget,
                    path: PathBuf::from("/plasmoids").join(name),
                    is_system: false,
                    release_date: String::new(),
                },
                diagnostic: Diagnostic::new(
                    name.to_string(),
                    "no download url available".to_string(),
                ),
            }
        }

        let mut result = UpdateCheckResult::default();
        result.no_matching_download.push(no_match("keep.widget"));
        result
            .no_matching_download
            .push(no_match("excluded.widget"));
        let config =
            crate::Config::new().with_excluded_packages(vec!["excluded.widget".to_string()]);

        let filtered = super::apply_exclusions(result, &config);

        assert_eq!(
            filtered.skipped,
            vec!["excluded.widget".to_string()],
            "an excluded component's name did not land in skipped"
        );
        let kept: Vec<&str> = filtered
            .no_matching_download
            .iter()
            .map(|entry| entry.component.name.as_str())
            .collect();
        assert_eq!(
            kept,
            vec!["keep.widget"],
            "an excluded no-match stayed a failure row"
        );
    }

    const CHILD_FLAG: &str = "PLASMOID_UPDATER_ENV_CHILD";
    const MARKER_VAR: &str = "PLASMOID_UPDATER_ENV_MARKER";

    /// The `which` binary the parent resolves, so a child `PATH` can hold it
    /// alone: tool lookups then run against a path with no tools in it.
    fn which_on_path() -> std::path::PathBuf {
        let path = std::env::var_os("PATH")
            .expect("PATH is unset; the child's tool lookups would fail without `which`");
        std::env::split_paths(&path)
            .map(|dir| dir.join("which"))
            .find(|candidate| candidate.is_file())
            .expect("no `which` on PATH; the child would read every tool as missing")
    }

    /// Runs the body in a child process whose `PATH` holds `which` and nothing
    /// else: setting env in-process needs `unsafe`, which the workspace forbids.
    #[test]
    fn check_does_not_require_bsdtar_but_installs_do() {
        if std::env::var_os(CHILD_FLAG).is_some() {
            std::fs::write(std::env::var(MARKER_VAR).unwrap(), b"ok").unwrap();
            assert!(
                super::check_dependency("which").is_ok(),
                "the child's PATH must resolve `which`, or every tool reads as missing"
            );
            assert!(
                super::validate_environment(true).is_ok(),
                "check must not need bsdtar on PATH"
            );
            assert!(
                matches!(
                    super::require_extractor(),
                    Err(crate::Error::MissingTool { tool: "bsdtar" })
                ),
                "update and install still need bsdtar"
            );
            return;
        }

        let sandbox = tempfile::tempdir().unwrap();
        let path = sandbox.path().join("bin");
        std::fs::create_dir(&path).unwrap();
        std::fs::copy(which_on_path(), path.join("which")).unwrap();
        let marker = sandbox.path().join("child-done");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "check_does_not_require_bsdtar_but_installs_do",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", &path)
            .env(CHILD_FLAG, "1")
            .env(MARKER_VAR, &marker)
            .output()
            .unwrap();
        assert!(
            output.status.success() && marker.exists(),
            "child run failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            marker.exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
