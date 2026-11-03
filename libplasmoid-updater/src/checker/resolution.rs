// SPDX-License-Identifier: GPL-3.0-or-later
//
// ID resolution approach based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};

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
/// 2. Exact name match from store API results, only against entries of the
///    component's own type
/// 3. Fallback widgets-id table
///
/// `None` when no tier matches.
pub(crate) fn resolve_content_id(
    component: &InstalledComponent,
    store_entries: &[StoreEntry],
    lookup: &IdLookup,
) -> Option<u64> {
    if let Some(id) = lookup.registry_id(component) {
        return Some(id);
    }

    resolve_by_name(component, store_entries)
        .or_else(|| resolve_by_table(component, lookup.widgets_id_table))
}

fn resolve_by_name(component: &InstalledComponent, store_entries: &[StoreEntry]) -> Option<u64> {
    let matching: Vec<&StoreEntry> = store_entries
        .iter()
        .filter(|entry| {
            entry.name.eq_ignore_ascii_case(&component.name)
                && component.component_type.matches_type_id(entry.type_id)
        })
        .collect();

    match matching.as_slice() {
        [] => None,
        [entry] => Some(entry.id),
        several => Some(newest_entry(component, several).id),
    }
}

/// Picks the entry to treat as the installed one when several store entries
/// share the component's name and type. Entries uploaded by the component's
/// own author win; otherwise, and when that still leaves several, the latest
/// `<changed>` instant wins, an undated (or unparseable) entry ranks below
/// every dated one, and an equal instant falls to the higher content id.
fn newest_entry<'a>(
    component: &InstalledComponent,
    candidates: &[&'a StoreEntry],
) -> &'a StoreEntry {
    let by_author = entries_by_the_author(component, candidates);
    let pool = if by_author.is_empty() {
        candidates
    } else {
        by_author.as_slice()
    };
    let mut newest = pool[0];
    for &entry in &pool[1..] {
        if outranks(entry, newest) {
            newest = entry;
        }
    }
    newest
}

/// The `<changed>` value as an instant: `None` when it is empty or not an
/// RFC 3339 timestamp.
fn changed_instant(entry: &StoreEntry) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(entry.changed_date.trim()).ok()
}

/// Whether `candidate` supersedes `incumbent` for the installed slot: a later
/// `<changed>` instant, any dated entry over an undated or unparseable one, and
/// a higher content id on equal instants (or when both are undated).
fn outranks(candidate: &StoreEntry, incumbent: &StoreEntry) -> bool {
    match (changed_instant(candidate), changed_instant(incumbent)) {
        (Some(candidate_at), Some(incumbent_at)) => {
            candidate_at > incumbent_at
                || (candidate_at == incumbent_at && candidate.id > incumbent.id)
        }
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => candidate.id > incumbent.id,
    }
}

/// The candidates uploaded by the component's own author, as named in its
/// metadata: empty when the metadata names no author or none matches.
fn entries_by_the_author<'a>(
    component: &InstalledComponent,
    candidates: &[&'a StoreEntry],
) -> Vec<&'a StoreEntry> {
    let Some(metadata) = super::discovery::read_metadata(&component.path) else {
        return Vec::new();
    };
    let authors = metadata.author_names();
    candidates
        .iter()
        .copied()
        .filter(|entry| {
            let uploader = entry.author.trim().to_lowercase();
            !uploader.is_empty() && authors.iter().any(|name| name.to_lowercase() == uploader)
        })
        .collect()
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

/// The one download link this entry offers for `target_version`.
///
/// A link whose version matches exactly wins, then one whose version matches
/// once normalized. With no match, the store's only link is taken when it
/// carries no version at all; a lone link naming another version, and several
/// links none of which match, are no match — an older or unrelated archive is
/// never installed as the requested version.
pub(crate) fn select_download_with_info(
    entry: &StoreEntry,
    target_version: &str,
) -> Option<DownloadInfo> {
    let candidates: Vec<_> = entry
        .download_links
        .iter()
        .filter(|l| !is_signature_file(&l.url))
        .collect();

    let normalized_target = normalize_version(target_version);
    let link = candidates
        .iter()
        .copied()
        .find(|l| l.version == target_version)
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|l| normalize_version(&l.version) == normalized_target)
        })
        .or(match candidates.as_slice() {
            [only] if only.version.is_empty() => Some(*only),
            _ => None,
        })?;

    Some(DownloadInfo {
        url: link.url.clone(),
        checksum: link.checksum.clone(),
        size_kb: link.size_kb,
    })
}

pub(crate) fn find_store_entry(entries: &[StoreEntry], content_id: u64) -> Option<&StoreEntry> {
    entries.iter().find(|e| e.id == content_id)
}

/// Resolution by name alone, without registry or table lookups.
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
        make_entry_by(id, name, type_id, "", "")
    }

    fn make_entry_by(
        id: u64,
        name: &str,
        type_id: u16,
        changed_date: &str,
        author: &str,
    ) -> StoreEntry {
        StoreEntry {
            id,
            name: name.to_string(),
            author: author.to_string(),
            version: "2.0.0".to_string(),
            type_id,
            download_links: vec![],
            changed_date: changed_date.to_string(),
        }
    }

    fn empty_lookup() -> (HashMap<String, u64>, crate::registry::RegistryIdCache) {
        (HashMap::new(), HashMap::new())
    }

    #[test]
    fn name_match_requires_a_matching_type() {
        let component = make_component(
            "My Widget",
            "org.example.widget",
            ComponentType::PlasmaWidget,
        );
        let entries = vec![
            make_entry(999, "My Widget", 716),
            make_entry(998, "My Widget", 722),
            make_entry(997, "My Widget", 714),
        ];
        let (wid, reg) = empty_lookup();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, None);
    }

    #[test]
    fn name_match_ignores_entries_of_other_types() {
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
    fn name_match_resolves_when_the_registry_has_no_entry() {
        let component = make_component(
            "Fancy Thing",
            "org.example.fancy",
            ComponentType::PlasmaWidget,
        );
        let entries = vec![make_entry(555, "Fancy Thing", 706)];
        let (wid, reg) = empty_lookup();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, Some(555));
    }

    fn component_with_metadata(authors: &str) -> (tempfile::TempDir, InstalledComponent) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("metadata.json"),
            format!("{{\"KPlugin\": {{\"Name\": \"Clock\", \"Authors\": [{authors}]}}}}"),
        )
        .unwrap();
        let mut component =
            make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        component.path = dir.path().to_path_buf();
        (dir, component)
    }

    fn resolve_with(component: &InstalledComponent, entries: &[StoreEntry]) -> Option<u64> {
        let (wid, reg) = empty_lookup();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };
        resolve_content_id(component, entries, &lookup)
    }

    #[test]
    fn same_named_entries_resolve_to_the_latest_changed() {
        let component = make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        let newest = make_entry_by(100, "Clock", 708, "2026-10-01T00:00:00+00:00", "");
        let older = make_entry_by(200, "Clock", 709, "2026-09-01T00:00:00+00:00", "");

        assert_eq!(
            resolve_with(&component, &[older.clone(), newest.clone()]),
            Some(100)
        );
        assert_eq!(resolve_with(&component, &[newest, older]), Some(100));
    }

    #[test]
    fn equal_changed_dates_resolve_to_the_higher_content_id() {
        let component = make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        let lower = make_entry_by(100, "Clock", 708, "2026-10-01T00:00:00+00:00", "");
        let higher = make_entry_by(200, "Clock", 709, "2026-10-01T00:00:00+00:00", "");

        assert_eq!(
            resolve_with(&component, &[higher.clone(), lower.clone()]),
            Some(200)
        );
        assert_eq!(resolve_with(&component, &[lower, higher]), Some(200));

        // The same instant written in two offsets: only instant equality ties
        // them, so a string comparison ranks the wrong one.
        let plus_two = make_entry_by(100, "Clock", 708, "2026-10-01T12:00:00+02:00", "");
        let utc = make_entry_by(200, "Clock", 709, "2026-10-01T10:00:00+00:00", "");
        assert_eq!(
            resolve_with(&component, &[plus_two.clone(), utc.clone()]),
            Some(200)
        );
        assert_eq!(resolve_with(&component, &[utc, plus_two]), Some(200));
    }

    #[test]
    fn same_day_uploads_resolve_to_the_later_timestamp() {
        let component = make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        // The later upload reads an earlier wall clock in its own offset, so
        // only comparing instants picks it.
        let earlier = make_entry_by(300, "Clock", 709, "2026-10-01T22:00:00+02:00", "");
        let later = make_entry_by(100, "Clock", 708, "2026-10-01T21:00:00+00:00", "");

        assert_eq!(
            resolve_with(&component, &[earlier.clone(), later.clone()]),
            Some(100)
        );
        assert_eq!(resolve_with(&component, &[later, earlier]), Some(100));
    }

    #[test]
    fn an_empty_or_unparseable_changed_never_outranks_a_dated_entry() {
        let component = make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        let dated = make_entry_by(100, "Clock", 708, "2026-10-01T00:00:00+00:00", "");
        let undated = make_entry_by(500, "Clock", 709, "", "");
        let unparseable = make_entry_by(600, "Clock", 709, "yesterday", "");

        assert_eq!(
            resolve_with(&component, &[undated.clone(), dated.clone()]),
            Some(100)
        );
        assert_eq!(
            resolve_with(&component, &[dated.clone(), undated]),
            Some(100)
        );
        assert_eq!(
            resolve_with(&component, &[unparseable.clone(), dated.clone()]),
            Some(100)
        );
        assert_eq!(resolve_with(&component, &[dated, unparseable]), Some(100));
    }

    #[test]
    fn the_installed_author_match_is_unicode_case_insensitive() {
        let (_dir, component) = component_with_metadata("{\"Name\": \"ÉLODIE\"}");
        let own_uploader = make_entry_by(100, "Clock", 709, "2026-09-01T00:00:00+00:00", "élodie");
        let other_uploader =
            make_entry_by(300, "Clock", 708, "2026-10-01T00:00:00+00:00", "someone");

        assert_eq!(
            resolve_with(&component, &[other_uploader.clone(), own_uploader.clone()]),
            Some(100)
        );
        assert_eq!(
            resolve_with(&component, &[own_uploader, other_uploader]),
            Some(100)
        );
    }

    #[test]
    fn same_named_entries_resolve_to_the_newest_without_readable_metadata() {
        let invalid_metadata = tempfile::tempdir().unwrap();
        std::fs::write(invalid_metadata.path().join("metadata.json"), "not json").unwrap();
        let missing_metadata = tempfile::tempdir().unwrap();
        let older = make_entry_by(300, "Clock", 709, "2026-09-01T00:00:00+00:00", "alpha");
        let newer = make_entry_by(200, "Clock", 708, "2026-10-01T00:00:00+00:00", "beta");

        let mut unreadable =
            make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        unreadable.path = invalid_metadata.path().to_path_buf();
        assert_eq!(
            resolve_with(&unreadable, &[older.clone(), newer.clone()]),
            Some(200),
            "an unparseable metadata.json must not stop resolution"
        );

        let mut absent = make_component("Clock", "org.example.clock", ComponentType::PlasmaWidget);
        absent.path = missing_metadata.path().to_path_buf();
        assert_eq!(
            resolve_with(&absent, &[older, newer]),
            Some(200),
            "a component with no metadata file must still resolve to the newest entry"
        );
    }

    #[test]
    fn the_installed_author_narrows_same_named_entries() {
        let (_dir, component) = component_with_metadata(
            "{\"Email\": \"oursbleu18@gmail.com\", \"Name\": \" Elwynn18 \"}",
        );
        let own_uploader =
            make_entry_by(100, "Clock", 709, "2026-09-01T00:00:00+00:00", "elwynn18");
        let other_uploader =
            make_entry_by(300, "Clock", 708, "2026-10-01T00:00:00+00:00", "someone");

        assert_eq!(
            resolve_with(&component, &[other_uploader.clone(), own_uploader.clone()]),
            Some(100)
        );
        assert_eq!(
            resolve_with(&component, &[own_uploader, other_uploader]),
            Some(100)
        );
    }

    #[test]
    fn an_unmatched_author_still_resolves_to_the_latest_changed() {
        let (_dir, component) = component_with_metadata("{\"Name\": \"Somebody Else\"}");
        let older = make_entry_by(300, "Clock", 709, "2026-09-01T00:00:00+00:00", "alpha");
        let newer = make_entry_by(200, "Clock", 708, "2026-10-01T00:00:00+00:00", "beta");

        assert_eq!(
            resolve_with(&component, &[older.clone(), newer.clone()]),
            Some(200)
        );
        assert_eq!(resolve_with(&component, &[newer, older]), Some(200));
    }

    fn entry_with_links(version: &str, links: &[(&str, &str)]) -> StoreEntry {
        StoreEntry {
            id: 1,
            name: "Test".to_string(),
            author: String::new(),
            version: version.to_string(),
            type_id: 705,
            download_links: links
                .iter()
                .map(|(url, version)| crate::types::DownloadLink {
                    url: (*url).to_string(),
                    version: (*version).to_string(),
                    checksum: None,
                    size_kb: None,
                })
                .collect(),
            changed_date: String::new(),
        }
    }

    #[test]
    fn a_lone_link_carrying_an_older_version_is_no_match() {
        let entry = entry_with_links("1.1.3", &[("https://example.com/a.tar.gz", "1.1.2")]);

        assert!(select_download_with_info(&entry, "1.1.3").is_none());
    }

    #[test]
    fn versioned_links_none_matching_are_no_match() {
        let entry = entry_with_links(
            "3.0",
            &[
                ("https://example.com/a.tar.gz", "1.0"),
                ("https://example.com/b.tar.gz", "2.0"),
            ],
        );

        assert!(select_download_with_info(&entry, "3.0").is_none());
    }

    #[test]
    fn several_links_none_matching_are_no_match_even_when_unversioned() {
        let entry = entry_with_links(
            "3.0",
            &[
                ("https://example.com/a.tar.gz", ""),
                ("https://example.com/b.tar.gz", ""),
            ],
        );

        assert!(select_download_with_info(&entry, "3.0").is_none());
    }

    #[test]
    fn a_lone_link_carrying_no_version_is_installed() {
        let entry = entry_with_links("1.1.3", &[("https://example.com/a.tar.gz", "")]);

        let info = select_download_with_info(&entry, "1.1.3").unwrap();
        assert_eq!(info.url, "https://example.com/a.tar.gz");
    }

    #[test]
    fn a_lone_unversioned_link_beside_a_signature_is_installed() {
        let entry = entry_with_links(
            "1.1.3",
            &[
                ("https://example.com/a.tar.gz", ""),
                ("https://example.com/a.tar.gz.sig", ""),
            ],
        );

        let info = select_download_with_info(&entry, "1.1.3").unwrap();
        assert_eq!(info.url, "https://example.com/a.tar.gz");
    }

    #[test]
    fn download_link_matches_with_normalized_version() {
        use crate::types::DownloadLink;

        let entry = StoreEntry {
            id: 1,
            name: "Test".to_string(),
            author: String::new(),
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
            author: String::new(),
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
        reg.insert(
            crate::registry::RegistryInstall {
                component_type: ComponentType::PlasmaWidget,
                path: component.path.clone(),
            },
            100,
        );
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };

        let result = resolve_content_id(&component, &entries, &lookup);
        assert_eq!(result, Some(100));
    }

    #[test]
    fn a_registry_id_answers_only_for_its_own_type_and_install_root() {
        let mut component = make_component("Breeze", "Breeze", ComponentType::IconTheme);
        component.path = PathBuf::from("/home/u/.local/share/icons/Breeze");
        let mut reg = HashMap::new();
        reg.insert(
            crate::registry::RegistryInstall {
                component_type: ComponentType::CursorTheme,
                path: PathBuf::from("/home/u/.icons/Breeze"),
            },
            107,
        );
        reg.insert(
            crate::registry::RegistryInstall {
                component_type: ComponentType::IconTheme,
                path: PathBuf::from("/home/u/.icons/Breeze"),
            },
            108,
        );
        let wid = HashMap::new();
        {
            let lookup = IdLookup {
                widgets_id_table: &wid,
                registry_id_cache: &reg,
            };
            assert_eq!(
                resolve_content_id(&component, &[], &lookup),
                None,
                "an id for another type or another install root answered"
            );
        }

        reg.insert(
            crate::registry::RegistryInstall {
                component_type: ComponentType::IconTheme,
                path: component.path.clone(),
            },
            132,
        );
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };
        assert_eq!(resolve_content_id(&component, &[], &lookup), Some(132));
    }

    #[test]
    fn download_link_filters_signature_files() {
        use crate::types::DownloadLink;
        let entry = StoreEntry {
            id: 1,
            name: "Test".to_string(),
            author: String::new(),
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
            author: String::new(),
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
