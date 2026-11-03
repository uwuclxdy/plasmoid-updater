// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{
    types::{AvailableUpdate, Diagnostic, InstalledComponent, StoreEntry},
    version,
};

use super::{IdLookup, resolution};

pub(crate) enum ComponentCheckResult {
    Update(Box<AvailableUpdate>),
    /// No store entry matches the component: resolution found no id, so the
    /// component cannot be checked. A `--system` check skips it; a user check
    /// reports it unresolved.
    Unmatched(Diagnostic),
    /// The resolved store entry is another component type, so the component
    /// has no store entry of its type: a `--system` check skips it; a user
    /// check keeps today's routing.
    TypeMismatch(Diagnostic),
    Unresolved(Diagnostic),
    /// The store entry offers no download link this component may take. The
    /// check reports the diagnostic; `update` turns the carried component into
    /// a failure, since a component it cannot download for is not skipped.
    NoMatchingDownload {
        component: InstalledComponent,
        diagnostic: Diagnostic,
    },
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
        return ComponentCheckResult::Unmatched(diagnostic);
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
            format!("could not fetch store entry {content_id}"),
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
    // Resolution returns a content id, not an entry type: the widgets-id table
    // is keyed by directory name alone, so the entry that id now names may
    // belong to another component type. Installing that archive would
    // overwrite the component with a different kind of package.
    if !component.component_type.matches_type_id(entry.type_id) {
        log::warn!(
            target: "resolver",
            "store entry {} is type {}, not {}: refusing it for '{}'",
            entry.id,
            entry.type_id,
            component.component_type,
            component.name,
        );
        let diagnostic = Diagnostic::new(
            component.name.clone(),
            format!("store entry {} is a different component type", entry.id),
        )
        .with_content_id(content_id);
        return ComponentCheckResult::TypeMismatch(diagnostic);
    }

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
            crate::types::NO_DOWNLOAD_URL_REASON.to_string(),
        )
        .with_versions(installed_version, available_version)
        .with_content_id(content_id);
        return ComponentCheckResult::NoMatchingDownload {
            component: component.clone(),
            diagnostic,
        };
    };

    let update = AvailableUpdate::builder(
        component.clone(),
        content_id,
        entry.version.clone(),
        download_info.url,
        entry.changed_date.clone(),
    )
    .checksum(download_info.checksum)
    .download_size(download_info.size_kb.and_then(|kb| kb.checked_mul(1024)))
    .build();

    ComponentCheckResult::Update(Box::new(update))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        registry::RegistryInstall,
        types::{ComponentType, DownloadLink},
    };
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
            author: String::new(),
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
    fn an_advertised_size_past_u64_bytes_does_not_overflow() {
        let component = make_component("Cool Widget", "org.example.cool");
        let mut entry = make_entry(222, "Cool Widget", "2.0.0", 705);
        entry.download_links[0].size_kb = Some(u64::MAX);

        // Reading the KiB count as bytes must neither panic (debug) nor wrap
        // to a small cap (release).
        let ComponentCheckResult::Update(update) = evaluate_store_entry(&component, &entry, 222)
        else {
            panic!("expected an update");
        };
        assert!(
            update
                .download_size
                .is_none_or(|size| size >= u64::MAX / 1024),
            "the size wrapped to {:?}",
            update.download_size
        );
    }

    #[test]
    fn stale_registry_id_falls_back_to_name_match() {
        let component = make_component("Cool Widget", "org.example.cool");
        let store_entries = vec![make_entry(222, "Cool Widget", "2.0.0", 705)];

        let mut reg = HashMap::new();
        reg.insert(
            RegistryInstall {
                component_type: ComponentType::PlasmaWidget,
                path: PathBuf::from("/tmp/test"),
            },
            111_u64,
        );
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
        reg.insert(
            RegistryInstall {
                component_type: ComponentType::PlasmaWidget,
                path: PathBuf::from("/tmp/test"),
            },
            100_u64,
        );
        let wid = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = check_component(&component, &store_entries, &lookup);
        assert!(matches!(result, ComponentCheckResult::Update(_)));
    }

    #[test]
    fn same_named_entries_evaluate_the_latest_changed() {
        let component = make_component("Clock", "org.example.clock");
        let mut older = make_entry(100, "Clock", "3.0.0", 708);
        older.changed_date = "2026-09-01T00:00:00+00:00".to_string();
        let mut newer = make_entry(200, "Clock", "2.0.0", 709);
        newer.changed_date = "2026-10-01T00:00:00+00:00".to_string();
        let store_entries = vec![older, newer];
        let wid = HashMap::new();
        let reg = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let ComponentCheckResult::Update(update) =
            check_component(&component, &store_entries, &lookup)
        else {
            panic!("expected an update from the latest changed entry");
        };
        assert_eq!(update.content_id, 200);
        assert_eq!(update.latest_version, "2.0.0");
    }

    #[test]
    fn stale_id_with_no_name_match_reports_unresolved() {
        let component = make_component("Missing Widget", "org.example.missing");
        let store_entries = vec![make_entry(222, "Other Widget", "2.0.0", 705)];

        let mut reg = HashMap::new();
        reg.insert(
            RegistryInstall {
                component_type: ComponentType::PlasmaWidget,
                path: PathBuf::from("/tmp/test"),
            },
            111_u64,
        );
        let wid = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = check_component(&component, &store_entries, &lookup);
        assert!(matches!(result, ComponentCheckResult::Unresolved(_)));
    }

    #[test]
    fn a_store_entry_of_another_type_is_never_evaluated_for_a_component() {
        let mut component = make_component("Shared", "Shared");
        component.component_type = ComponentType::IconTheme;
        let foreign = make_entry(500, "Shared", "2.0.0", 107);

        assert!(
            !matches!(
                evaluate_store_entry(&component, &foreign, 500),
                ComponentCheckResult::Update(_)
            ),
            "a cursor theme entry was evaluated as an icon theme update"
        );

        let own = make_entry(501, "Shared", "2.0.0", 132);
        assert!(
            matches!(
                evaluate_store_entry(&component, &own, 501),
                ComponentCheckResult::Update(_)
            ),
            "an icon theme entry was refused for an icon theme"
        );
    }

    /// A component no store entry matches is `Unmatched`, not `Unresolved`:
    /// the two route differently for a `--system` check.
    #[test]
    fn a_component_with_no_store_entry_is_unmatched() {
        let component = make_component("Distro Widget", "org.example.distro");
        let wid = HashMap::new();
        let reg = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = check_component(&component, &[], &lookup);

        assert!(
            matches!(result, ComponentCheckResult::Unmatched(_)),
            "expected Unmatched"
        );
    }
}
