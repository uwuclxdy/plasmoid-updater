// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use crate::{
    types::{AvailableUpdate, ComponentDiagnostic, InstalledComponent, StoreEntry},
    version,
};

use super::resolution;

pub(crate) enum ComponentCheckResult {
    Update(Box<AvailableUpdate>),
    Unresolved(ComponentDiagnostic),
    CheckFailed(ComponentDiagnostic),
    UpToDate,
}

/// Evaluates a store entry against a component to determine if an update is available based on version and release date.
pub(crate) fn check_component(
    component: &InstalledComponent,
    store_entries: &[StoreEntry],
    widgets_id_table: &HashMap<String, u64>,
    registry_id_cache: &HashMap<String, u64>,
) -> ComponentCheckResult {
    let Some(content_id) = resolution::resolve_content_id(
        component,
        store_entries,
        widgets_id_table,
        registry_id_cache,
    ) else {
        let version_str = if component.version.is_empty() {
            "<empty>"
        } else {
            &component.version
        };
        log::debug!(
            target: "resolver",
            "could not resolve id for '{}' (version: {})",
            component.name,
            version_str,
        );
        let installed_version = (!component.version.is_empty()).then(|| component.version.clone());
        let diagnostic = ComponentDiagnostic::new(
            component.name.clone(),
            "could not match to kde store entry".to_string(),
        )
        .with_versions(installed_version, None);
        return ComponentCheckResult::Unresolved(diagnostic);
    };

    let Some(entry) = resolution::find_store_entry(store_entries, content_id) else {
        log::debug!(
            target: "resolver",
            "store entry not found for id {} ({})",
            content_id,
            component.name
        );
        let diagnostic = ComponentDiagnostic::new(
            component.name.clone(),
            format!("store entry {content_id} not in fetched data"),
        )
        .with_content_id(content_id);
        return ComponentCheckResult::Unresolved(diagnostic);
    };

    evaluate_store_entry(component, entry, content_id)
}

/// Shared logic for evaluating a store entry against an installed component.
///
/// Performs version/date comparison and download URL selection, returning
/// the appropriate check result. Used by both regular and registry-based
/// component processing paths.
pub(crate) fn evaluate_store_entry(
    component: &InstalledComponent,
    entry: &StoreEntry,
    content_id: u64,
) -> ComponentCheckResult {
    if !version::is_update_available_with_date(
        &component.version,
        &entry.version,
        &component.release_date,
        &entry.changed_date,
    ) {
        return ComponentCheckResult::UpToDate;
    }

    let Some(download_info) = resolution::select_download_with_info(entry, &entry.version) else {
        log::warn!(
            target: "resolver",
            "no download url for '{}' (id: {})",
            component.name,
            content_id
        );
        let installed_version = (!component.version.is_empty()).then(|| component.version.clone());
        let available_version = (!entry.version.is_empty()).then(|| entry.version.clone());
        let diagnostic = ComponentDiagnostic::new(
            component.name.clone(),
            "no download url available".to_string(),
        )
        .with_versions(installed_version, available_version)
        .with_content_id(content_id);
        return ComponentCheckResult::CheckFailed(diagnostic);
    };

    let update = AvailableUpdate::builder(
        component.clone(),
        content_id,
        entry.version.clone(),
        download_info.url,
        entry.changed_date.clone(),
    )
    .checksum(download_info.checksum)
    .download_size(download_info.size_kb.map(|kb| kb * 1024))
    .build();

    ComponentCheckResult::Update(Box::new(update))
}
