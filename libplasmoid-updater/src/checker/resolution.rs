// SPDX-License-Identifier: MIT OR Apache-2.0
//
// ID resolution approach based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License

use std::collections::HashMap;

use crate::types::{InstalledComponent, StoreEntry};

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
    widgets_id_table: &HashMap<String, u64>,
    registry_id_cache: &HashMap<String, u64>,
) -> Option<u64> {
    registry_id_cache
        .get(&component.directory_name)
        .copied()
        .or_else(|| resolve_by_name(component, store_entries))
        .or_else(|| resolve_by_table(component, widgets_id_table))
}

fn resolve_by_name(component: &InstalledComponent, store_entries: &[StoreEntry]) -> Option<u64> {
    store_entries
        .iter()
        .find(|e| e.name == component.name && component.component_type.matches_type_id(e.type_id))
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
