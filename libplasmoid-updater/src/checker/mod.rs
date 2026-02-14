// SPDX-License-Identifier: MIT OR Apache-2.0

mod discovery;
mod evaluation;
mod registry;
mod resolution;
mod store;

use rayon::prelude::*;

use crate::{Result, api::ApiClient, config::Config, types::UpdateCheckResult};

pub use discovery::find_installed;
pub use resolution::{find_store_entry, select_download_url, select_download_with_info};

/// Checks for updates for all installed Plasmoids.
pub(crate) fn check(
    config: &Config,
    system: bool,
    api_client: &ApiClient,
) -> Result<UpdateCheckResult> {
    let components = find_installed(system)?;

    if components.is_empty() {
        return Ok(UpdateCheckResult::new());
    }

    let (registry_components, regular_components) = store::partition_components(components);
    let regular_types = store::scannable_types(system);

    let store_entries =
        store::fetch_store_entries(api_client, &regular_types, &regular_components)?;
    let registry_id_cache = crate::registry::build_id_cache();

    let mut result = UpdateCheckResult::new();

    let regular_results: Vec<evaluation::ComponentCheckResult> = regular_components
        .par_iter()
        .map(|component| {
            evaluation::check_component(
                component,
                &store_entries,
                &config.widgets_id_table,
                &registry_id_cache,
            )
        })
        .collect();

    for check_result in regular_results {
        match check_result {
            evaluation::ComponentCheckResult::Update(update) => result.add_update(*update),
            evaluation::ComponentCheckResult::Unresolved(diagnostic) => {
                result.add_unresolved(diagnostic);
            }
            evaluation::ComponentCheckResult::CheckFailed(diagnostic) => {
                result.add_check_failure(diagnostic);
            }
            evaluation::ComponentCheckResult::UpToDate => {}
        }
    }

    registry::check_components(
        &registry_components,
        api_client,
        &store_entries,
        &config.widgets_id_table,
        &registry_id_cache,
        &mut result,
    );

    Ok(result)
}
