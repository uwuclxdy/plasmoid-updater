// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::exit_code::ExitCode;
use crate::output::{
    Verbosity, output_json, print_components_table, print_count_message, print_info,
};
use libplasmoid_updater::list_installed;

pub fn execute(
    system: bool,
    json: bool,
    verbosity: Verbosity,
) -> Result<ExitCode, libplasmoid_updater::Error> {
    let components = list_installed(system)?;

    if json {
        return output_json(&components);
    }

    if components.is_empty() {
        print_info(verbosity, "no components installed");
        return Ok(ExitCode::Success);
    }

    print_count_message(verbosity, components.len(), "installed component");
    if verbosity != Verbosity::Quiet {
        print_components_table(&components, verbosity);
    }

    Ok(ExitCode::Success)
}
