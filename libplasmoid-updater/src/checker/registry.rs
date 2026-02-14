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
    let resolved: Vec<_> = registry_components
        .iter()
        .filter_map(|c| {
            resolution::resolve_content_id(c, store_entries, widgets_id_table, registry_id_cache)
                .map(|id| (c, id))
        })
        .collect();

    let content_ids: Vec<u64> = resolved.iter().map(|(_, id)| *id).collect();
    let fetched_entries = client.fetch_details(&content_ids);

    for ((component, content_id), fetch_result) in resolved.iter().zip(fetched_entries) {
        match fetch_result {
            Ok(entry) => match evaluation::evaluate_store_entry(component, &entry, *content_id) {
                evaluation::ComponentCheckResult::Update(update) => result.add_update(*update),
                evaluation::ComponentCheckResult::CheckFailed(diagnostic) => {
                    result.add_check_failure(diagnostic);
                }
                evaluation::ComponentCheckResult::UpToDate => {}
                evaluation::ComponentCheckResult::Unresolved(_) => {
                    unreachable!("evaluate_store_entry never returns Unresolved")
                }
            },
            Err(e) => {
                let diagnostic = ComponentDiagnostic::new(
                    component.name.clone(),
                    format!("failed to fetch store entry: {e}"),
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
