// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(feature = "cli")]
use crate::cli::{self, progress::create_fetch_spinner};
#[cfg(feature = "cli")]
use inquire::InquireError;

use crate::{FailedUpdate, UnverifiedUpdate};
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use std::sync::Arc;

use crate::{
    Config, Error, RestartBehavior, UpdateResult,
    api::ApiClient,
    checker::{check_with_components, find_installed},
    installer,
    types::{AvailableUpdate, InstalledComponent, UpdateCheckResult},
};

pub(crate) fn validate_environment(skip_plasma_detection: bool) -> crate::Result<()> {
    if cfg!(not(target_os = "linux")) {
        return Err(Error::UnsupportedOS(std::env::consts::OS.to_string()));
    }
    let plasma_found = skip_plasma_detection || crate::paths::is_kde();
    if !plasma_found {
        return Err(Error::NotKDE);
    }
    check_dependency("bsdtar")?;
    Ok(())
}

fn check_dependency(name: &str) -> crate::Result<()> {
    use std::process::Command;
    match Command::new("which").arg(name).output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err(Error::MissingDependency(name.to_string())),
    }
}

pub(crate) fn fetch_updates(
    api_client: &ApiClient,
    config: &Config,
) -> crate::Result<UpdateCheckResult> {
    #[cfg(feature = "cli")]
    let spinner = create_fetch_spinner();

    let components = find_installed(config.system)?;
    let result = check_with_components(config, api_client, components)?;

    #[cfg(feature = "cli")]
    spinner.finish_and_clear();

    Ok(result)
}

pub(crate) fn select_updates<'a>(
    updates: &'a [AvailableUpdate],
    config: &Config,
) -> crate::Result<Vec<&'a AvailableUpdate>> {
    let matcher = ExcludeMatcher::new(&config.excluded_packages, &config.excluded_patterns);

    #[cfg(feature = "cli")]
    if !config.auto_confirm && stdin_is_terminal() {
        return prompt_update_selection(updates, &matcher);
    }

    Ok(filter_excluded(updates, &matcher))
}

pub(crate) fn filter_excluded<'a>(
    updates: &'a [AvailableUpdate],
    matcher: &ExcludeMatcher,
) -> Vec<&'a AvailableUpdate> {
    updates.iter().filter(|u| !matcher.is_excluded(u)).collect()
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
    pub(crate) fn new(exact: &'a [String], patterns: &[String]) -> Self {
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

#[cfg(feature = "cli")]
pub(crate) fn stdin_is_terminal() -> bool {
    use is_terminal::IsTerminal;
    std::io::stdin().is_terminal()
}

#[cfg(feature = "cli")]
pub(crate) fn prompt_update_selection<'a>(
    updates: &'a [AvailableUpdate],
    matcher: &ExcludeMatcher,
) -> crate::Result<Vec<&'a AvailableUpdate>> {
    let options = format_menu_options(updates);

    let defaults: Vec<usize> = updates
        .iter()
        .enumerate()
        .filter(|(_, u)| !matcher.is_excluded(u))
        .map(|(i, _)| i)
        .collect();

    let plural = if updates.len() == 1 { "" } else { "s" };
    let prompt = format!(
        "{} update{plural} available, select to apply:",
        updates.len()
    );

    match inquire::MultiSelect::new(&prompt, options)
        .with_default(&defaults)
        .with_page_size(15)
        .raw_prompt()
    {
        Ok(selected) => {
            let result: Vec<&AvailableUpdate> = selected
                .into_iter()
                .map(|opt| &updates[opt.index])
                .collect();

            use std::io::Write;
            print!("{}", cli::CLEAR_LINE_SEQUENCE);
            std::io::stdout().flush().ok();

            Ok(result)
        }
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(vec![]),
        Err(e) => Err(crate::Error::other(format!("prompt failed: {e}"))),
    }
}

#[cfg(feature = "cli")]
pub(crate) fn format_menu_options(updates: &[AvailableUpdate]) -> Vec<String> {
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
                cli::output::format_version(&u.installed.version),
                cli::output::format_version(&u.latest_version),
            )
        })
        .collect()
}

pub(crate) fn install_selected_updates(
    updates: &[&AvailableUpdate],
    api_client: &ApiClient,
    config: &Config,
) -> crate::Result<(UpdateResult, Vec<InstalledComponent>)> {
    let result = Arc::new(parking_lot::Mutex::new(UpdateResult::default()));
    let restarted_components = Arc::new(parking_lot::Mutex::new(Vec::new()));

    let _inhibit = if config.inhibit_idle {
        installer::InhibitGuard::acquire()
    } else {
        installer::InhibitGuard::None
    };

    #[cfg(feature = "cli")]
    let ui = cli::update_ui::UpdateUi::new(updates);

    let mut pool_builder = rayon::ThreadPoolBuilder::new();
    if let Some(thread_count) = config.threads {
        pool_builder = pool_builder.num_threads(thread_count);
    }
    let pool = pool_builder
        .build()
        .map_err(|e| Error::other(format!("failed to build rayon thread pool: {e}")))?;

    let counter = api_client.request_counter();

    pool.install(|| {
        updates.par_iter().enumerate().for_each(|(index, update)| {
            #[cfg(not(feature = "cli"))]
            let _ = index;
            let name = update.installed.name.clone();

            #[cfg(feature = "cli")]
            let reporter = ui.reporter(index);
            #[cfg(not(feature = "cli"))]
            let reporter = |_: u8| {};

            match installer::update_component(update, api_client.http_client(), reporter, &counter)
            {
                Ok(outcome) => {
                    #[cfg(feature = "cli")]
                    ui.complete_task(index, true);
                    restarted_components.lock().push(update.installed.clone());
                    let mut r = result.lock();
                    if !outcome.verified {
                        r.unverified.push(UnverifiedUpdate {
                            name: name.clone(),
                            expected_version: outcome.expected_version,
                            actual_version: outcome.actual_version,
                        });
                    }
                    r.succeeded.push(name);
                }
                Err(e) => {
                    #[cfg(feature = "cli")]
                    ui.complete_task(index, false);
                    result.lock().failed.push(FailedUpdate {
                        name,
                        error: e.to_string(),
                    });
                }
            }
        });
    });

    #[cfg(feature = "cli")]
    ui.finish();

    let result = Arc::into_inner(result).ok_or_else(|| {
        Error::other("install result still had multiple owners after thread pool completion")
    })?;
    let updated_components = Arc::into_inner(restarted_components).ok_or_else(|| {
        Error::other(
            "updated component list still had multiple owners after thread pool completion",
        )
    })?;

    Ok((result.into_inner(), updated_components.into_inner()))
}

pub(crate) fn handle_restart(config: &Config, updated_components: &[InstalledComponent]) {
    if updated_components.is_empty() {
        return;
    }

    if !installer::any_requires_restart(updated_components) {
        return;
    }

    match config.restart {
        RestartBehavior::Never => {}
        RestartBehavior::Always => {
            if let Err(e) = installer::restart_plasmashell() {
                log::warn!(target: "restart", "failed to restart plasmashell: {e}");
            }
        }
        #[cfg(feature = "cli")]
        RestartBehavior::Prompt => {
            if stdin_is_terminal() {
                prompt_restart();
            }
        }
        #[cfg(not(feature = "cli"))]
        RestartBehavior::Prompt => {
            // Without CLI, cannot prompt — fall back to not restarting
            log::info!(target: "restart", "prompt restart requested but no CLI available, skipping");
        }
    }
}

#[cfg(feature = "cli")]
pub(crate) fn prompt_restart() {
    match inquire::Confirm::new("Restart plasmashell now?")
        .with_default(false)
        .prompt()
    {
        Ok(true) => {
            if let Err(e) = installer::restart_plasmashell() {
                log::warn!(target: "restart", "failed to restart plasmashell: {e}");
            }
        }
        Ok(false) | Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {}
        Err(e) => log::warn!(target: "restart", "prompt failed: {e}"),
    }
}

#[cfg(feature = "cli")]
pub(crate) fn display_check_results(result: &crate::types::UpdateCheckResult) {
    if result.updates.is_empty() {
        println!("no updates available");
        return;
    }

    cli::output::print_count_message(result.updates.len(), "update");
    cli::output::print_updates_table(&result.updates);
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
