// SPDX-License-Identifier: GPL-3.0-or-later
//
// ID resolution approach based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License

use std::collections::HashMap;

use crate::types::{InstalledComponent, StoreEntry};

use super::IdLookup;

pub(crate) struct DownloadInfo {
    pub(crate) url: String,
    pub(crate) checksum: Option<String>,
    pub(crate) size_kb: Option<u64>,
}

/// Resolves the KDE Store content ID for an installed component.
///
/// Uses a three-tier resolution strategy:
/// 1. KNewStuff registry lookup via pre-built cache (most reliable)
/// 2. Exact name match from store API results
/// 3. Fallback widgets-id table
pub(crate) fn resolve_content_id(
    component: &InstalledComponent,
    store_entries: &[StoreEntry],
    lookup: &IdLookup,
) -> Option<u64> {
    lookup
        .registry_id_cache
        .get(&component.directory_name)
        .copied()
        .or_else(|| resolve_by_name(component, store_entries))
        .or_else(|| resolve_by_table(component, lookup.widgets_id_table))
}

fn resolve_by_name(component: &InstalledComponent, store_entries: &[StoreEntry]) -> Option<u64> {
    // Prefer entries that match both name and type
    let type_match = store_entries
        .iter()
        .find(|e| e.name == component.name && component.component_type.matches_type_id(e.type_id));

    if let Some(entry) = type_match {
        return Some(entry.id);
    }

    // Fall back to name-only match (handles miscategorized store entries)
    store_entries
        .iter()
        .find(|e| e.name == component.name)
        .map(|e| e.id)
}

fn resolve_by_table(
    component: &InstalledComponent,
    widgets_id_table: &HashMap<String, u64>,
) -> Option<u64> {
    widgets_id_table.get(&component.directory_name).copied()
}

pub(crate) fn select_download_with_info(
    entry: &StoreEntry,
    target_version: &str,
) -> Option<DownloadInfo> {
    if entry.download_links.is_empty() {
        return None;
    }

    let link = if entry.download_links.len() == 1 {
        &entry.download_links[0]
    } else {
        entry
            .download_links
            .iter()
            .find(|l| l.version == target_version)
            .or_else(|| entry.download_links.first())?
    };

    Some(DownloadInfo {
        url: link.url.clone(),
        checksum: link.checksum.clone(),
        size_kb: link.size_kb,
    })
}

pub(crate) fn find_store_entry(entries: &[StoreEntry], content_id: u64) -> Option<&StoreEntry> {
    entries.iter().find(|e| e.id == content_id)
}

/// Name-only resolution without registry/table lookups.
/// Used as a fallback when the primary resolved ID is absent from fetched data.
#[expect(dead_code, reason = "will be used by stale-ID fallback in a follow-up change")]
pub(crate) fn resolve_by_name_only(
    component: &InstalledComponent,
    store_entries: &[StoreEntry],
) -> Option<u64> {
    resolve_by_name(component, store_entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ComponentType, InstalledComponent};
    use std::path::PathBuf;

    fn make_component(name: &str, dir_name: &str, ct: ComponentType) -> InstalledComponent {
        InstalledComponent {
            name: name.to_string(),
            directory_name: dir_name.to_string(),
            version: "1.0.0".to_string(),
            component_type: ct,
            path: PathBuf::from("/tmp/test"),
            is_system: false,
            release_date: String::new(),
        }
    }

    fn make_entry(id: u64, name: &str, type_id: u16) -> StoreEntry {
        StoreEntry {
            id,
            name: name.to_string(),
            version: "2.0.0".to_string(),
            type_id,
            download_links: vec![],
            changed_date: String::new(),
        }
    }

    fn empty_lookup() -> (HashMap<String, u64>, HashMap<String, u64>) {
        (HashMap::new(), HashMap::new())
    }

    #[test]
    fn name_match_ignores_type_id() {
        let component =
            make_component("My Widget", "org.example.widget", ComponentType::PlasmaWidget);
        let entries = vec![make_entry(999, "My Widget", 714)];
        let (wid, reg) = empty_lookup();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, Some(999));
    }

    #[test]
    fn name_match_prefers_same_type_when_ambiguous() {
        let component =
            make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        let entries = vec![make_entry(100, "Clock", 708), make_entry(200, "Clock", 299)];
        let (wid, reg) = empty_lookup();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, Some(100));
    }

    #[test]
    fn name_match_falls_back_to_any_type_when_no_type_match() {
        let component =
            make_component("Fancy Thing", "org.example.fancy", ComponentType::KWinEffect);
        let entries = vec![make_entry(555, "Fancy Thing", 705)];
        let (wid, reg) = empty_lookup();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, Some(555));
    }

    #[test]
    fn registry_cache_takes_priority_over_name() {
        let component =
            make_component("My Widget", "org.example.widget", ComponentType::PlasmaWidget);
        let entries = vec![make_entry(200, "My Widget", 705)];
        let wid = HashMap::new();
        let mut reg = HashMap::new();
        reg.insert("org.example.widget".to_string(), 100);
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, Some(100));
    }
}
