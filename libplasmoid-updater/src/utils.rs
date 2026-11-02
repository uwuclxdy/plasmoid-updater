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
    check_dependency("bsdtar")?;
    Ok(())
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
) -> crate::Result<UpdateCheckResult> {
    let components = find_installed(config.system)?;
    check_with_components(config, api_client, components)
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
            patterns: compile_exclude_patterns(patterns),
        }
    }

    pub(crate) fn is_excluded(&self, update: &AvailableUpdate) -> bool {
        let dir = &update.installed.directory_name;
        let name = &update.installed.name;

        if self.exact.iter().any(|e| e == dir || e == name) {
            return true;
        }

        match &self.patterns {
            Some(set) => set.is_match(dir) || set.is_match(name),
            None => false,
        }
    }
}

/// Compiles exclusion patterns into a single [`regex::RegexSet`].
///
/// Each pattern is validated individually so a single bad pattern only drops
/// itself (with a warning) instead of discarding the whole set. Returns `None`
/// when no usable pattern remains, letting the matcher skip regex work entirely.
fn compile_exclude_patterns(patterns: &[String]) -> Option<regex::RegexSet> {
    let valid: Vec<&str> = patterns
        .iter()
        .filter(|p| match regex::Regex::new(p) {
            Ok(_) => true,
            Err(e) => {
                log::warn!(target: "config", "ignoring invalid exclude pattern {p:?}: {e}");
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
/// leave `updates` and their names land in `skipped`. This is the one place
/// exclusions are applied; every later stage works on the filtered list.
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

    let outcomes: Vec<(InstalledComponent, Result<InstallOutcome, ComponentFailure>)> = pool
        .install(|| {
            updates
                .par_iter()
                .map(|update| {
                    let component = update.installed.clone();
                    let outcome =
                        installer::update_component(update, api_client.http_client(), reporter);
                    report_outcome(&component, &outcome, reporter);
                    (component, outcome)
                })
                .collect()
        });

    let mut succeeded = Vec::new();
    let mut failed = Vec::new();

    for (component, outcome) in outcomes {
        match outcome {
            Ok(outcome) if outcome.verified => succeeded.push(component),
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
    use super::compile_exclude_patterns;

    #[test]
    fn compile_keeps_valid_patterns_and_matches_both_substrings_and_anchors() {
        let set = compile_exclude_patterns(&[r"^org\.kde\.".to_string(), "weather".to_string()])
            .expect("two valid patterns should compile");

        assert!(set.is_match("org.kde.plasma.systemmonitor"));
        assert!(set.is_match("my-weather-widget"));
        assert!(!set.is_match("com.example.clock"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn compile_drops_only_the_invalid_pattern() {
        let set = compile_exclude_patterns(&[
            "valid".to_string(),
            "(".to_string(), // unbalanced group — invalid
        ])
        .expect("the one valid pattern should survive");

        assert_eq!(set.len(), 1);
        assert!(set.is_match("a valid name"));
    }

    #[test]
    fn compile_returns_none_when_empty_or_all_invalid() {
        assert!(compile_exclude_patterns(&[]).is_none());
        assert!(compile_exclude_patterns(&["(".to_string()]).is_none());
    }
}
