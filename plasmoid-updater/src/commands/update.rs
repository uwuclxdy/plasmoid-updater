// SPDX-License-Identifier: MIT OR Apache-2.0

use is_terminal::IsTerminal;

use crate::cli_config::CliConfig;
use crate::exit_code::ExitCode;
use crate::output::{
    Verbosity, format_version, output_error, output_json, output_json_error,
    print_count_message, print_info, print_non_interactive_hint, print_updates_table,
};
use crate::progress;
use libplasmoid_updater::{
    ApiClient, AvailableUpdate, UpdateSummary, check_updates, restart_plasmashell,
    update_component,
};

pub struct Options<'a> {
    pub component: Option<&'a str>,
    pub restart_plasma: bool,
    pub no_restart_plasma: bool,
    pub yes: bool,
    pub verbosity: Verbosity,
}

pub fn execute(
    system: bool,
    json: bool,
    config: &CliConfig,
    options: Options,
    api_client: &ApiClient,
) -> Result<ExitCode, libplasmoid_updater::Error> {
    let updates = fetch_updates(system, json, config, options.verbosity, api_client)?;

    if updates.is_empty() {
        return handle_no_updates(json, options.verbosity);
    }

    let to_update = select_components(&updates, &options, config, json)?;

    if to_update.is_empty() {
        return handle_no_selection(json, options.component, options.verbosity);
    }

    let summary = execute_updates(&to_update, json, options.verbosity, api_client)?;

    if json {
        output_json(&summary)?;
    }

    handle_restart(&to_update, &summary, config, &options, json)?;

    Ok(exit_code_from_summary(&summary))
}

fn fetch_updates(
    system: bool,
    json: bool,
    config: &CliConfig,
    verbosity: Verbosity,
    api_client: &ApiClient,
) -> Result<Vec<AvailableUpdate>, libplasmoid_updater::Error> {
    let feedback = !json && verbosity != Verbosity::Quiet;

    let _spinner = if feedback {
        Some(progress::create_fetch_spinner())
    } else {
        None
    };

    let result = check_updates(&config.inner, system, api_client)?;

    if let Some(spinner) = _spinner {
        spinner.finish_and_clear();
    }

    Ok(result.updates)
}

fn handle_no_updates(
    json: bool,
    verbosity: Verbosity,
) -> Result<ExitCode, libplasmoid_updater::Error> {
    if json {
        return output_json(UpdateSummary::default());
    }
    print_info(verbosity, "no updates available");
    Ok(ExitCode::Success)
}

fn select_components<'a>(
    updates: &'a [AvailableUpdate],
    options: &Options,
    config: &CliConfig,
    json: bool,
) -> Result<Vec<&'a AvailableUpdate>, libplasmoid_updater::Error> {
    if let Some(name) = options.component {
        return Ok(filter_by_name(updates, name));
    }

    if config.update_all_by_default || config.assume_yes || options.yes || json {
        return Ok(filter_excluded(updates, &config.excluded_packages));
    }

    select_interactive(updates, config, options.verbosity)
}

fn filter_by_name<'a>(updates: &'a [AvailableUpdate], name: &str) -> Vec<&'a AvailableUpdate> {
    updates
        .iter()
        .filter(|u| {
            u.installed.name.eq_ignore_ascii_case(name)
                || u.installed.directory_name == name
                || u.content_id.to_string() == name
        })
        .collect()
}

fn filter_excluded<'a>(
    updates: &'a [AvailableUpdate],
    excluded: &[String],
) -> Vec<&'a AvailableUpdate> {
    updates
        .iter()
        .filter(|u| !is_excluded(u, excluded))
        .collect()
}

fn is_excluded(update: &AvailableUpdate, excluded: &[String]) -> bool {
    excluded
        .iter()
        .any(|e| e == &update.installed.directory_name || e == &update.installed.name)
}

fn handle_no_selection(
    json: bool,
    component: Option<&str>,
    verbosity: Verbosity,
) -> Result<ExitCode, libplasmoid_updater::Error> {
    if component.is_some() {
        if json {
            return output_json_error("component not found or no update available");
        }
        output_error(false, "component not found or no update available");
        return Ok(ExitCode::PartialFailure);
    }

    if json {
        return output_json(UpdateSummary::default());
    }
    print_info(verbosity, "no updates to apply (all excluded)");
    Ok(ExitCode::Success)
}

fn execute_updates(
    to_update: &[&AvailableUpdate],
    json: bool,
    verbosity: Verbosity,
    api_client: &ApiClient,
) -> Result<UpdateSummary, libplasmoid_updater::Error> {
    let mut summary = UpdateSummary::default();

    for update in to_update {
        process_update(update, &mut summary, json, verbosity, api_client);
    }

    Ok(summary)
}

fn process_update(
    update: &AvailableUpdate,
    summary: &mut UpdateSummary,
    json: bool,
    verbosity: Verbosity,
    api_client: &ApiClient,
) {
    let name = update.installed.name.clone();
    let feedback = !json && verbosity != Verbosity::Quiet;

    let _spinner = if feedback {
        Some(progress::create_component_spinner(&name))
    } else {
        None
    };

    match update_component(update, api_client.http_client()) {
        Ok(()) => {
            if let Some(spinner) = _spinner {
                spinner.finish_and_clear();
                progress::print_update_success(
                    &name,
                    &update.installed.version,
                    &update.latest_version,
                );
            }
            summary.add_success(name);
        }
        Err(e) => {
            if let Some(spinner) = _spinner {
                spinner.finish_and_clear();
                progress::print_update_failure(&name);
            }
            summary.add_failure(name, e.to_string());
        }
    }
}

fn select_interactive<'a>(
    updates: &'a [AvailableUpdate],
    config: &CliConfig,
    verbosity: Verbosity,
) -> Result<Vec<&'a AvailableUpdate>, libplasmoid_updater::Error> {
    if !std::io::stdin().is_terminal() {
        show_non_interactive_message(updates, verbosity);
        return Ok(vec![]);
    }

    let available = filter_excluded(updates, &config.excluded_packages);

    if available.is_empty() {
        show_all_excluded_message(verbosity);
        return Ok(vec![]);
    }

    prompt_selection(&available)
}

fn show_non_interactive_message(updates: &[AvailableUpdate], verbosity: Verbosity) {
    if verbosity == Verbosity::Quiet {
        return;
    }

    print_count_message(verbosity, updates.len(), "update");
    print_updates_table(updates, verbosity);
    print_non_interactive_hint(updates.len());
}

fn show_all_excluded_message(verbosity: Verbosity) {
    print_info(verbosity, "no updates to apply (all excluded)");
}

fn prompt_selection<'a>(
    available: &[&'a AvailableUpdate],
) -> Result<Vec<&'a AvailableUpdate>, libplasmoid_updater::Error> {
    let options = format_options(available);
    let defaults: Vec<usize> = (0..options.len()).collect();
    let plural = if available.len() == 1 { "" } else { "s" };
    let msg = format!(
        "{} update{} available, select to apply:",
        available.len(),
        plural
    );

    match inquire::MultiSelect::new(&msg, options)
        .with_default(&defaults)
        .with_page_size(15)
        .raw_prompt()
    {
        Ok(selected) => Ok(selected
            .into_iter()
            .map(|opt| available[opt.index])
            .collect()),
        Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(vec![]),
        Err(e) => Err(libplasmoid_updater::Error::other(format!(
            "prompt failed: {e}"
        ))),
    }
}

fn format_options(available: &[&AvailableUpdate]) -> Vec<String> {
    let nw = available
        .iter()
        .map(|u| u.installed.name.len())
        .max()
        .unwrap_or(10)
        .max(10);

    available
        .iter()
        .map(|u| {
            format!(
                "{:<nw$}  {} -> {}",
                u.installed.name,
                format_version(&u.installed.version),
                format_version(&u.latest_version)
            )
        })
        .collect()
}

fn handle_restart(
    updates: &[&AvailableUpdate],
    summary: &UpdateSummary,
    config: &CliConfig,
    options: &Options,
    json: bool,
) -> Result<(), libplasmoid_updater::Error> {
    if requires_restart(updates) && !summary.succeeded.is_empty() && !json {
        perform_restart_if_needed(config, options.restart_plasma, options.no_restart_plasma)?;
    }
    Ok(())
}

fn requires_restart(updates: &[&AvailableUpdate]) -> bool {
    updates
        .iter()
        .any(|u| libplasmoid_updater::installer::requires_plasmashell_restart(&u.installed))
}

fn perform_restart_if_needed(
    config: &CliConfig,
    restart_plasma: bool,
    no_restart_plasma: bool,
) -> Result<(), libplasmoid_updater::Error> {
    if no_restart_plasma {
        return Ok(());
    }

    if restart_plasma {
        return do_restart();
    }

    if config.prompt_restart && std::io::stdin().is_terminal() {
        return prompt_restart();
    }

    Ok(())
}

fn do_restart() -> Result<(), libplasmoid_updater::Error> {
    restart_plasmashell()
}

fn prompt_restart() -> Result<(), libplasmoid_updater::Error> {
    match inquire::Confirm::new("Restart plasmashell now?")
        .with_default(false)
        .prompt()
    {
        Ok(true) => do_restart(),
        Ok(false)
        | Err(
            inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted,
        ) => Ok(()),
        Err(e) => Err(libplasmoid_updater::Error::other(format!(
            "prompt failed: {e}"
        ))),
    }
}

fn exit_code_from_summary(summary: &UpdateSummary) -> ExitCode {
    if summary.has_failures() {
        ExitCode::PartialFailure
    } else {
        ExitCode::Success
    }
}
