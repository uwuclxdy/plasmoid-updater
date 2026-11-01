// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};

use crate::{
    Error,
    api::ApiClient,
    types::{Diagnostic, InstalledComponent, StoreEntry, UpdateCheckResult},
};

use super::{IdLookup, evaluation, resolution};

/// Checks if any of the components from the widget-id registry table have updates available.
pub(crate) fn check_components(
    registry_components: &[InstalledComponent],
    client: &ApiClient,
    store_entries: &[StoreEntry],
    lookup: &IdLookup,
    result: &mut UpdateCheckResult,
) -> crate::Result<()> {
    let resolved: Vec<(&InstalledComponent, u64)> = registry_components
        .iter()
        .filter_map(|c| resolution::resolve_content_id(c, store_entries, lookup).map(|id| (c, id)))
        .collect();

    // Reuse any entries already present in store_entries; fetch only the rest.
    let missing_ids: Vec<u64> = {
        let mut seen = HashSet::new();
        resolved
            .iter()
            .filter(|(_, id)| resolution::find_store_entry(store_entries, *id).is_none())
            .filter(|(_, id)| seen.insert(*id))
            .map(|(_, id)| *id)
            .collect()
    };

    // A per-id lookup failure (a gone entry, a bad status, a timeout, a
    // dropped connection) degrades that component to a diagnostic below.
    // Only a lookup that could not connect to the store at all proves the
    // store unreachable, and only when every lookup failed that way and no
    // store entry is available from anywhere else — then the run fails.
    let mut fetched: HashMap<u64, StoreEntry> = HashMap::new();
    let mut connect_failures = 0usize;
    let mut last_connect_error: Option<Error> = None;
    for (id, result) in missing_ids.iter().zip(client.fetch_details(&missing_ids)) {
        match result {
            Ok(entry) => {
                fetched.insert(*id, entry);
            }
            Err(e) => {
                log::warn!(target: "resolver", "store entry {id} unavailable: {e}");
                if matches!(&e, Error::Network(err) if err.is_connect()) {
                    connect_failures += 1;
                    last_connect_error = Some(e);
                }
            }
        }
    }

    if let Some(err) = last_connect_error
        && connect_failures == missing_ids.len()
        && fetched.is_empty()
        && store_entries.is_empty()
    {
        return Err(err);
    }

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
                evaluation::ComponentCheckResult::Unresolved(diagnostic) => {
                    result.add_check_failure(diagnostic);
                }
            },
            None => {
                let diagnostic = Diagnostic::new(
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
            let diagnostic = Diagnostic::new(
                component.name.clone(),
                "could not match to kde store entry".to_string(),
            );
            result.add_unresolved(diagnostic);
        }
    }

    Ok(())
}
