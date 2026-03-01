// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use crate::{
    api::ApiClient,
    types::{ComponentDiagnostic, InstalledComponent, StoreEntry, UpdateCheckResult},
};

use super::{evaluation, resolution};

/// Checks if any of the components from the widget-id registry table have updates available.
pub(crate) fn check_components(
    registry_components: &[InstalledComponent],
    client: &ApiClient,
    store_entries: &[StoreEntry],
    widgets_id_table: &HashMap<String, u64>,
    registry_id_cache: &HashMap<String, u64>,
    result: &mut UpdateCheckResult,
) {
    let resolved: Vec<(&InstalledComponent, u64)> = registry_components
        .iter()
        .filter_map(|c| {
            resolution::resolve_content_id(c, store_entries, widgets_id_table, registry_id_cache)
                .map(|id| (c, id))
        })
        .collect();

    // Reuse any entries already present in store_entries; fetch only the rest.
    let missing_ids: Vec<u64> = resolved
        .iter()
        .filter(|(_, id)| resolution::find_store_entry(store_entries, *id).is_none())
        .map(|(_, id)| *id)
        .collect();

    let fetched: HashMap<u64, StoreEntry> = client
        .fetch_details(&missing_ids)
        .into_iter()
        .zip(missing_ids.iter())
        .filter_map(|(r, &id)| r.ok().map(|e| (id, e)))
        .collect();

    for (component, content_id) in &resolved {
        let entry = resolution::find_store_entry(store_entries, *content_id)
            .or_else(|| fetched.get(content_id));

        match entry {
            Some(entry) => match evaluation::evaluate_store_entry(component, entry, *content_id) {
                evaluation::ComponentCheckResult::Update(update) => {
                    result.add_update(*update);
                }
                evaluation::ComponentCheckResult::CheckFailed(diagnostic) => {
                    result.add_check_failure(diagnostic);
                }
                evaluation::ComponentCheckResult::UpToDate => {}
                evaluation::ComponentCheckResult::Unresolved(_) => {
                    unreachable!("evaluate_store_entry never returns Unresolved")
                }
            },
            None => {
                let diagnostic = ComponentDiagnostic::new(
                    component.name.clone(),
                    "failed to fetch store entry".to_string(),
                )
                .with_content_id(*content_id);
                result.add_check_failure(diagnostic);
            }
        }
    }

    for component in registry_components {
        if !resolved
            .iter()
            .any(|(c, _)| c.directory_name == component.directory_name)
        {
            let diagnostic = ComponentDiagnostic::new(
                component.name.clone(),
                "could not match to kde store entry".to_string(),
            );
            result.add_unresolved(diagnostic);
        }
    }
}
