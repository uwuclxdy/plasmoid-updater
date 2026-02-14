// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::cli_config::CliConfig;
use crate::exit_code::ExitCode;
use crate::output::{Verbosity, output_json, print_count_message, print_info, print_updates_table};
use crate::progress;
use libplasmoid_updater::{ApiClient, check_updates};

pub fn execute(
    system: bool,
    json: bool,
    config: &CliConfig,
    verbosity: Verbosity,
    api_client: &ApiClient,
) -> Result<ExitCode, libplasmoid_updater::Error> {
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

    if json {
        return output_json(&result);
    }

    if result.updates.is_empty() {
        print_info(verbosity, "no updates available");
        return Ok(ExitCode::Success);
    }

    print_count_message(verbosity, result.updates.len(), "update");
    if verbosity != Verbosity::Quiet {
        print_updates_table(&result.updates, verbosity);
    }

    Ok(ExitCode::Success)
}
