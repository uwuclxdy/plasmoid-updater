#[cfg(feature = "cli")]
use crate::cli::{self, progress::create_fetch_spinner};
#[cfg(feature = "cli")]
use inquire::InquireError;

use std::sync::Arc;

use crate::{
    CheckError, Config, RestartBehavior, UpdateResult,
    api::ApiClient,
    checker::{check_with_components, find_installed},
    installer,
    types::{AvailableUpdate, UpdateCheckResult},
};

pub(crate) fn validate_environment() -> std::result::Result<(), CheckError> {
    if cfg!(not(target_os = "linux")) {
        return Err(CheckError::UnsupportedOS(std::env::consts::OS.to_string()));
    }
    if !crate::paths::is_kde() {
        return Err(CheckError::NotKDE);
    }
    Ok(())
}

pub(crate) fn fetch_updates(
    api_client: &ApiClient,
    config: &Config,
) -> std::result::Result<UpdateCheckResult, CheckError> {
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
    #[cfg(feature = "cli")]
    if !config.yes && stdin_is_terminal() {
        return prompt_update_selection(updates, &config.excluded_packages);
    }

    Ok(filter_excluded(updates, &config.excluded_packages))
}

pub(crate) fn filter_excluded<'a>(
    updates: &'a [AvailableUpdate],
    excluded: &[String],
) -> Vec<&'a AvailableUpdate> {
    updates
        .iter()
        .filter(|u| !is_excluded(u, excluded))
        .collect()
}

pub(crate) fn is_excluded(update: &AvailableUpdate, excluded: &[String]) -> bool {
    excluded
        .iter()
        .any(|e| e == &update.installed.directory_name || e == &update.installed.name)
}

#[cfg(feature = "cli")]
pub(crate) fn stdin_is_terminal() -> bool {
    use is_terminal::IsTerminal;
    std::io::stdin().is_terminal()
}

#[cfg(feature = "cli")]
pub(crate) fn prompt_update_selection<'a>(
    updates: &'a [AvailableUpdate],
    excluded: &[String],
) -> crate::Result<Vec<&'a AvailableUpdate>> {
    let options = format_menu_options(updates);

    let defaults: Vec<usize> = updates
        .iter()
        .enumerate()
        .filter(|(_, u)| !is_excluded(u, excluded))
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
) -> crate::Result<UpdateResult> {
    let result = Arc::new(parking_lot::Mutex::new(UpdateResult::default()));

    #[cfg(feature = "cli")]
    let ui = cli::update_ui::UpdateUi::new(updates);

    // 0 = rayon default = number of logical CPUs
    let thread_count = config.threads.unwrap_or(0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(thread_count)
        .build()
        .unwrap_or_else(|_| {
            rayon::ThreadPoolBuilder::new()
                .build()
                .expect("failed to build default thread pool")
        });

    let counter = api_client.request_counter();

    pool.install(|| {
        use rayon::prelude::*;
        updates.par_iter().enumerate().for_each(|(index, update)| {
            let name = update.installed.name.clone();

            #[cfg(feature = "cli")]
            let reporter = ui.reporter(index);
            #[cfg(not(feature = "cli"))]
            let reporter = |_: u8| {};

            match installer::update_component(update, api_client.http_client(), reporter, &counter)
            {
                Ok(()) => {
                    #[cfg(feature = "cli")]
                    ui.complete_task(index, true);
                    result.lock().succeeded.push(name);
                }
                Err(e) => {
                    #[cfg(feature = "cli")]
                    ui.complete_task(index, false);
                    result.lock().failed.push((name, e.to_string()));
                }
            }
        });
    });

    #[cfg(feature = "cli")]
    ui.finish();

    Ok(Arc::try_unwrap(result).unwrap().into_inner())
}

pub(crate) fn handle_restart(config: &Config, updates: &[AvailableUpdate], result: &UpdateResult) {
    if result.succeeded.is_empty() {
        return;
    }
    if !installer::any_requires_restart(updates) {
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
