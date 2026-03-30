// SPDX-License-Identifier: GPL-3.0-or-later
//
// ID resolution approach based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License

use std::collections::HashMap;

use crate::types::{InstalledComponent, StoreEntry};
use crate::version::normalize_version;

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
    let type_match = store_entries.iter().find(|e| {
        e.name.eq_ignore_ascii_case(&component.name)
            && component.component_type.matches_type_id(e.type_id)
    });

    if let Some(entry) = type_match {
        return Some(entry.id);
    }

    // Fall back to name-only match (handles miscategorized store entries)
    store_entries
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(&component.name))
        .map(|e| e.id)
}

fn resolve_by_table(
    component: &InstalledComponent,
    widgets_id_table: &HashMap<String, u64>,
) -> Option<u64> {
    widgets_id_table.get(&component.directory_name).copied()
}

/// Returns true if the URL points to a detached signature file rather than an archive.
fn is_signature_file(url: &str) -> bool {
    url.ends_with(".asc") || url.ends_with(".sig")
}

pub(crate) fn select_download_with_info(
    entry: &StoreEntry,
    target_version: &str,
) -> Option<DownloadInfo> {
    if entry.download_links.is_empty() {
        return None;
    }

    let candidates: Vec<_> = entry
        .download_links
        .iter()
        .filter(|l| !is_signature_file(&l.url))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    let link = if candidates.len() == 1 {
        candidates[0]
    } else {
        let normalized_target = normalize_version(target_version);
        // Prefer exact match, then normalized match, then first link
        candidates
            .iter()
            .find(|l| l.version == target_version)
            .or_else(|| {
                candidates
                    .iter()
                    .find(|l| normalize_version(&l.version) == normalized_target)
            })
            .or_else(|| candidates.first())
            .copied()?
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
        let component = make_component(
            "My Widget",
            "org.example.widget",
            ComponentType::PlasmaWidget,
        );
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
        let component = make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
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
        let component = make_component(
            "Fancy Thing",
            "org.example.fancy",
            ComponentType::KWinEffect,
        );
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
    fn download_link_matches_with_normalized_version() {
        use crate::types::DownloadLink;

        let entry = StoreEntry {
            id: 1,
            name: "Test".to_string(),
            version: "2.0.0".to_string(),
            type_id: 705,
            download_links: vec![
                DownloadLink {
                    url: "https://example.com/old.tar.gz".to_string(),
                    version: "v1.0.0".to_string(),
                    checksum: None,
                    size_kb: None,
                },
                DownloadLink {
                    url: "https://example.com/new.tar.gz".to_string(),
                    version: "v2.0.0".to_string(),
                    checksum: None,
                    size_kb: None,
                },
            ],
            changed_date: String::new(),
        };

        let result = select_download_with_info(&entry, "2.0.0");
        assert!(result.is_some());
        assert_eq!(result.unwrap().url, "https://example.com/new.tar.gz");
    }

    #[test]
    fn download_link_exact_match_preferred() {
        use crate::types::DownloadLink;

        let entry = StoreEntry {
            id: 1,
            name: "Test".to_string(),
            version: "2.0.0".to_string(),
            type_id: 705,
            download_links: vec![
                DownloadLink {
                    url: "https://example.com/a.tar.gz".to_string(),
                    version: "2.0.0".to_string(),
                    checksum: None,
                    size_kb: None,
                },
                DownloadLink {
                    url: "https://example.com/b.tar.gz".to_string(),
                    version: "2.0.0".to_string(),
                    checksum: None,
                    size_kb: None,
                },
            ],
            changed_date: String::new(),
        };

        let result = select_download_with_info(&entry, "2.0.0");
        assert!(result.is_some());
        assert_eq!(result.unwrap().url, "https://example.com/a.tar.gz");
    }

    #[test]
    fn registry_cache_takes_priority_over_name() {
        let component = make_component(
            "My Widget",
            "org.example.widget",
            ComponentType::PlasmaWidget,
        );
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

    #[test]
    fn download_link_filters_signature_files() {
        use crate::types::DownloadLink;
        let entry = StoreEntry {
            id: 1,
            name: "Test".to_string(),
            version: "1.0.0".to_string(),
            type_id: 705,
            download_links: vec![
                DownloadLink {
                    url: "https://example.com/pkg.tar.gz.asc".to_string(),
                    version: "1.0.0".to_string(),
                    checksum: None,
                    size_kb: None,
                },
                DownloadLink {
                    url: "https://example.com/pkg.tar.gz".to_string(),
                    version: "1.0.0".to_string(),
                    checksum: None,
                    size_kb: None,
                },
            ],
            changed_date: String::new(),
        };
        let result = select_download_with_info(&entry, "1.0.0");
        assert!(result.is_some());
        assert!(!result.unwrap().url.ends_with(".asc"));
    }

    #[test]
    fn download_link_returns_none_if_only_signature() {
        use crate::types::DownloadLink;
        let entry = StoreEntry {
            id: 1,
            name: "Test".to_string(),
            version: "1.0.0".to_string(),
            type_id: 705,
            download_links: vec![DownloadLink {
                url: "https://example.com/pkg.tar.gz.asc".to_string(),
                version: "1.0.0".to_string(),
                checksum: None,
                size_kb: None,
            }],
            changed_date: String::new(),
        };
        let result = select_download_with_info(&entry, "1.0.0");
        assert!(result.is_none());
    }

    #[test]
    fn name_match_is_case_insensitive() {
        let component = make_component(
            "My Widget",
            "org.example.widget",
            ComponentType::PlasmaWidget,
        );
        let entries = vec![make_entry(42, "my widget", 705)];
        let (wid, reg) = empty_lookup();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };
        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, Some(42));
    }
}
