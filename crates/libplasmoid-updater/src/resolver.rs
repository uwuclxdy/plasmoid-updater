// SPDX-License-Identifier: MIT OR Apache-2.0
//
// ID resolution approach based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License

use std::{collections::HashMap, fs};

use crate::{InstalledComponent, StoreEntry, registry};

pub(crate) struct DownloadInfo {
    pub(crate) url: String,
    pub(crate) checksum: Option<String>,
    pub(crate) size_kb: Option<u64>,
}

/// resolves the kde store content id for an installed component.
/// priority order:
/// 1. knewstuff registry (most reliable - already contains the id from discovery)
/// 2. exact name match from store api
/// 3. fallback widgets-id table
pub fn resolve_content_id(
    component: &InstalledComponent,
    store_entries: &[StoreEntry],
    widgets_id_table: &HashMap<String, u64>,
) -> Option<u64> {
    resolve_by_registry(component)
        .or_else(|| resolve_by_name(component, store_entries))
        .or_else(|| resolve_by_table(component, widgets_id_table))
}

fn resolve_by_name(component: &InstalledComponent, store_entries: &[StoreEntry]) -> Option<u64> {
    store_entries
        .iter()
        .find(|e| e.name == component.name)
        .map(|e| e.id)
}

fn resolve_by_registry(component: &InstalledComponent) -> Option<u64> {
    let registry_file = component.component_type.registry_file()?;
    let registry_path = crate::paths::knewstuff_dir().join(registry_file);

    if !registry_path.exists() {
        return None;
    }

    let content = fs::read_to_string(&registry_path).ok()?;
    registry::find_id_in_registry(&content, &component.directory_name)
}

fn resolve_by_table(
    component: &InstalledComponent,
    widgets_id_table: &HashMap<String, u64>,
) -> Option<u64> {
    widgets_id_table.get(&component.directory_name).copied()
}

pub fn select_download_url(entry: &StoreEntry, target_version: &str) -> Option<String> {
    select_download_with_info(entry, target_version).map(|i| i.url)
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

pub fn find_store_entry(entries: &[StoreEntry], content_id: u64) -> Option<&StoreEntry> {
    entries.iter().find(|e| e.id == content_id)
}
