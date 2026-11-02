// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;

use crate::{
    Result,
    api::ApiClient,
    types::{ComponentType, InstalledComponent, StoreEntry},
};

use super::IdLookup;

/// The store entries a check could read, and whether the store answered any
/// request at all.
pub(crate) struct StoreFetch {
    pub(crate) entries: Vec<StoreEntry>,
    pub(crate) answered: bool,
}

pub(crate) fn partition_components(
    components: Vec<InstalledComponent>,
) -> (Vec<InstalledComponent>, Vec<InstalledComponent>) {
    components
        .into_iter()
        .partition(|c| c.component_type.registry_only())
}

/// Fetches the minimum set of store entries needed to evaluate `regular_components`,
/// ensuring each entry is retrieved at most once.
///
/// Strategy:
/// 1. Resolve content IDs from local data (registry cache + widgets-id table) — no network.
/// 2. Fetch catalog pages for every distinct component type present, regardless of
///    whether IDs are already known. A single catalog page covers ≤100 entries,
///    converting O(n) targeted fetches into O(distinct_types) catalog requests.
/// 3. For known IDs genuinely absent from the catalog, issue one targeted request per ID.
pub(crate) fn fetch_store_entries(
    client: &ApiClient,
    regular_components: &[InstalledComponent],
    lookup: &IdLookup,
) -> Result<StoreFetch> {
    if regular_components.is_empty() {
        return Ok(StoreFetch {
            entries: Vec::new(),
            answered: false,
        });
    }

    let known_ids: HashSet<u64> = regular_components
        .iter()
        .filter_map(|c| resolve_id_locally(c, lookup))
        .collect();

    // Always fetch catalog for all distinct component types — not just unresolved ones.
    // When all IDs are locally known, skipping this forces one targeted request per ID.
    let types = distinct_types(regular_components);
    let catalog = client.fetch_all(&types);
    let mut answered = catalog.answered;

    // Targeted fetch only for known IDs genuinely absent from the catalog
    // (e.g. old/unlisted components that no longer appear in recent pages).
    let listed_ids: HashSet<u64> = catalog.entries.iter().map(|e| e.id).collect();
    let missing_ids: Vec<u64> = known_ids
        .iter()
        .copied()
        .filter(|id| !listed_ids.contains(id))
        .collect();

    let mut entries = catalog.entries;
    for (id, detail) in missing_ids.iter().zip(client.fetch_details(&missing_ids)) {
        answered |= detail.answered;
        match detail.result {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                log::warn!(target: "api", "store entry {id} could not be fetched: {error}");
            }
        }
    }

    if !answered && let Some(error) = catalog.error {
        return Err(error);
    }

    Ok(StoreFetch { entries, answered })
}

fn resolve_id_locally(component: &InstalledComponent, lookup: &IdLookup) -> Option<u64> {
    lookup
        .registry_id_cache
        .get(&component.directory_name)
        .copied()
        .or_else(|| {
            lookup
                .widgets_id_table
                .get(&component.directory_name)
                .copied()
        })
}

fn distinct_types(components: &[InstalledComponent]) -> Vec<ComponentType> {
    let mut seen = HashSet::new();
    components
        .iter()
        .map(|c| c.component_type)
        .filter(|t| seen.insert(*t))
        .collect()
}
