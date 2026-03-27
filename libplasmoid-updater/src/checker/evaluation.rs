// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{
    types::{AvailableUpdate, Diagnostic, InstalledComponent, StoreEntry},
    version,
};

use super::{IdLookup, resolution};

pub(crate) enum ComponentCheckResult {
    Update(Box<AvailableUpdate>),
    Unresolved(Diagnostic),
    CheckFailed(Diagnostic),
    UpToDate,
}

/// Evaluates a store entry against a component to determine if an update is available based on version and release date.
pub(crate) fn check_component(
    component: &InstalledComponent,
    store_entries: &[StoreEntry],
    lookup: &IdLookup,
) -> ComponentCheckResult {
    let Some(content_id) = resolution::resolve_content_id(component, store_entries, lookup) else {
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
        let diagnostic = Diagnostic::new(
            component.name.clone(),
            "could not match to kde store entry".to_string(),
        )
        .with_versions(installed_version, None);
        return ComponentCheckResult::Unresolved(diagnostic);
    };

    // Try to find the entry by resolved ID; if not found, retry with name match.
    // This handles stale registry entries pointing to delisted/re-uploaded content.
    let entry = resolution::find_store_entry(store_entries, content_id).or_else(|| {
        log::debug!(
            target: "resolver",
            "registry id {} not in catalog for '{}', retrying name match",
            content_id,
            component.name
        );
        resolution::resolve_by_name_only(component, store_entries)
            .and_then(|fallback_id| resolution::find_store_entry(store_entries, fallback_id))
    });

    let Some(entry) = entry else {
        log::debug!(
            target: "resolver",
            "store entry not found for id {} ({})",
            content_id,
            component.name
        );
        let diagnostic = Diagnostic::new(
            component.name.clone(),
            format!("store entry {content_id} not in fetched data"),
        )
        .with_content_id(content_id);
        return ComponentCheckResult::Unresolved(diagnostic);
    };

    evaluate_store_entry(component, entry, entry.id)
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
        let diagnostic = Diagnostic::new(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ComponentType, DownloadLink};
    use std::{collections::HashMap, path::PathBuf};

    fn make_component(name: &str, dir_name: &str) -> InstalledComponent {
        InstalledComponent {
            name: name.to_string(),
            directory_name: dir_name.to_string(),
            version: "1.0.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path: PathBuf::from("/tmp/test"),
            is_system: false,
            release_date: "2024-01-01".to_string(),
        }
    }

    fn make_entry(id: u64, name: &str, version: &str, type_id: u16) -> StoreEntry {
        StoreEntry {
            id,
            name: name.to_string(),
            version: version.to_string(),
            type_id,
            download_links: vec![DownloadLink {
                url: "https://example.com/download.tar.gz".to_string(),
                version: version.to_string(),
                checksum: None,
                size_kb: None,
            }],
            changed_date: "2025-06-01".to_string(),
        }
    }

    #[test]
    fn stale_registry_id_falls_back_to_name_match() {
        let component = make_component("Cool Widget", "org.example.cool");
        let store_entries = vec![make_entry(222, "Cool Widget", "2.0.0", 705)];

        let mut reg = HashMap::new();
        reg.insert("org.example.cool".to_string(), 111_u64);
        let wid = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = check_component(&component, &store_entries, &lookup);
        assert!(matches!(result, ComponentCheckResult::Update(_)));
    }

    #[test]
    fn valid_registry_id_still_works() {
        let component = make_component("My Widget", "org.example.widget");
        let store_entries = vec![make_entry(100, "My Widget", "2.0.0", 705)];

        let mut reg = HashMap::new();
        reg.insert("org.example.widget".to_string(), 100_u64);
        let wid = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = check_component(&component, &store_entries, &lookup);
        assert!(matches!(result, ComponentCheckResult::Update(_)));
    }

    #[test]
    fn stale_id_with_no_name_match_reports_unresolved() {
        let component = make_component("Missing Widget", "org.example.missing");
        let store_entries = vec![make_entry(222, "Other Widget", "2.0.0", 705)];

        let mut reg = HashMap::new();
        reg.insert("org.example.missing".to_string(), 111_u64);
        let wid = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = check_component(&component, &store_entries, &lookup);
        assert!(matches!(result, ComponentCheckResult::Unresolved(_)));
    }
}
