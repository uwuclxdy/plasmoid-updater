// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::{HashMap, HashSet};

use crate::{
    Result,
    api::ApiClient,
    types::{ComponentType, InstalledComponent, StoreEntry},
};

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
    widgets_id_table: &HashMap<String, u64>,
    registry_id_cache: &HashMap<String, u64>,
) -> Result<Vec<StoreEntry>> {
    if regular_components.is_empty() {
        return Ok(Vec::new());
    }

    let known_ids: Vec<u64> = regular_components
        .iter()
        .filter_map(|c| resolve_id_locally(c, widgets_id_table, registry_id_cache))
        .collect();

    // Always fetch catalog for all distinct component types — not just unresolved ones.
    // When all IDs are locally known, skipping this forces one targeted request per ID.
    let types = distinct_types(regular_components);
    let catalog_entries = client.fetch_all(&types)?;

    // Targeted fetch only for known IDs genuinely absent from the catalog
    // (e.g. old/unlisted components that no longer appear in recent pages).
    let catalog_ids: HashSet<u64> = catalog_entries.iter().map(|e| e.id).collect();
    let missing_ids: Vec<u64> = known_ids
        .into_iter()
        .filter(|id| !catalog_ids.contains(id))
        .collect();

    let targeted_entries: Vec<StoreEntry> = if !missing_ids.is_empty() {
        client
            .fetch_details(&missing_ids)
            .into_iter()
            .filter_map(|r| r.ok())
            .collect()
    } else {
        Vec::new()
    };

    Ok(catalog_entries
        .into_iter()
        .chain(targeted_entries)
        .collect())
}

fn resolve_id_locally(
    component: &InstalledComponent,
    widgets_id_table: &HashMap<String, u64>,
    registry_id_cache: &HashMap<String, u64>,
) -> Option<u64> {
    registry_id_cache
        .get(&component.directory_name)
        .copied()
        .or_else(|| widgets_id_table.get(&component.directory_name).copied())
}

fn distinct_types(components: &[InstalledComponent]) -> Vec<ComponentType> {
    let mut seen = HashSet::new();
    components
        .iter()
        .map(|c| c.component_type)
        .filter(|t| seen.insert(*t))
        .collect()
}
