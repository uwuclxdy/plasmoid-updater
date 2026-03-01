// SPDX-License-Identifier: MIT OR Apache-2.0

mod discovery;
mod evaluation;
mod registry;
mod resolution;
mod store;

use rayon::prelude::*;

use crate::{Result, api::ApiClient, config::Config, types::UpdateCheckResult};

pub(crate) use discovery::find_installed;

/// Checks for updates using pre-discovered components.
pub(crate) fn check_with_components(
    config: &Config,
    api_client: &ApiClient,
    components: Vec<crate::types::InstalledComponent>,
) -> Result<UpdateCheckResult> {
    if components.is_empty() {
        return Ok(UpdateCheckResult::default());
    }

    let (registry_components, regular_components) = store::partition_components(components);

    // Build local caches before any network call so fetch_store_entries
    // can resolve known IDs without touching the paginated catalog.
    let registry_id_cache = crate::registry::build_id_cache();

    let store_entries = store::fetch_store_entries(
        api_client,
        &regular_components,
        &config.widgets_id_table,
        &registry_id_cache,
    )?;

    let mut result = UpdateCheckResult::default();

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
