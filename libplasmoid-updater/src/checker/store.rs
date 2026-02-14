// SPDX-License-Identifier: MIT OR Apache-2.0

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

pub(crate) fn scannable_types(system: bool) -> Vec<ComponentType> {
    let all = if system {
        ComponentType::all()
    } else {
        ComponentType::all_user()
    };
    all.iter().filter(|t| !t.registry_only()).copied().collect()
}

pub(crate) fn fetch_store_entries(
    client: &ApiClient,
    regular_types: &[ComponentType],
    regular_components: &[InstalledComponent],
) -> Result<Vec<StoreEntry>> {
    if !regular_types.is_empty() && !regular_components.is_empty() {
        client.fetch_all(regular_types)
    } else {
        Ok(Vec::new())
    }
}
