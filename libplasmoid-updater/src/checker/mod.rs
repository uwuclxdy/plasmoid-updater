// SPDX-License-Identifier: GPL-3.0-or-later

mod discovery;
mod evaluation;
mod registry;
mod resolution;
mod store;

use std::collections::HashMap;

use crate::{Result, api::ApiClient, config::Config, types::UpdateCheckResult};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

pub(crate) use discovery::find_installed;

/// Pre-built lookup tables for resolving component content IDs.
///
/// Bundles the two hash maps that are threaded through every check function,
/// reducing parameter count across the checker module.
pub(crate) struct IdLookup<'a> {
    pub widgets_id_table: &'a HashMap<String, u64>,
    pub registry_id_cache: &'a HashMap<String, u64>,
}

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

    let lookup = IdLookup {
        widgets_id_table: &config.widgets_id_table,
        registry_id_cache: &registry_id_cache,
    };

    let store_entries = store::fetch_store_entries(api_client, &regular_components, &lookup)?;

    let mut result = UpdateCheckResult::default();

    let regular_results: Vec<evaluation::ComponentCheckResult> = regular_components
        .par_iter()
        .map(|component| evaluation::check_component(component, &store_entries, &lookup))
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
        &lookup,
        &mut result,
    );

    Ok(result)
}
