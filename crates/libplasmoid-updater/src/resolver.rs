// SPDX-License-Identifier: MIT OR Apache-2.0
//
// ID resolution approach based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License

use std::{collections::HashMap, fs, path::PathBuf};

use quick_xml::{Reader, events::Event};

use crate::{InstalledComponent, StoreEntry};

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
    let registry_path = get_knewstuff_dir().join(registry_file);

    if !registry_path.exists() {
        return None;
    }

    let content = fs::read_to_string(&registry_path).ok()?;
    find_id_in_registry(&content, &component.directory_name)
}

fn resolve_by_table(
    component: &InstalledComponent,
    widgets_id_table: &HashMap<String, u64>,
) -> Option<u64> {
    widgets_id_table.get(&component.directory_name).copied()
}

fn get_knewstuff_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("knewstuff3")
}

pub(crate) fn find_id_in_registry(xml: &str, directory_name: &str) -> Option<u64> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut current_element = String::new();
    let mut in_entry = false;
    let mut current_id: Option<u64> = None;
    let mut current_path: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = name.clone();

                if name == "stuff" {
                    in_entry = true;
                    current_id = None;
                    current_path = None;
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();

                if name == "stuff" && in_entry {
                    if let (Some(id), Some(path)) = (current_id, &current_path)
                        && path_matches_directory(path, directory_name)
                    {
                        return Some(id);
                    }
                    in_entry = false;
                }
            }
            Ok(Event::Text(e)) => {
                if !in_entry {
                    continue;
                }

                let text = String::from_utf8_lossy(e.as_ref()).to_string();

                match current_element.as_str() {
                    "id" => current_id = text.parse().ok(),
                    "installedfile" | "uninstalledfile" => current_path = Some(text),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    None
}

pub(crate) fn path_matches_directory(path: &str, directory_name: &str) -> bool {
    path.split('/').any(|segment| segment == directory_name)
}

pub fn select_download_url(entry: &StoreEntry, target_version: &str) -> Option<String> {
    if entry.download_links.is_empty() {
        return None;
    }

    if entry.download_links.len() == 1 {
        return Some(entry.download_links[0].url.clone());
    }

    entry
        .download_links
        .iter()
        .find(|link| link.version == target_version)
        .or_else(|| entry.download_links.first())
        .map(|link| link.url.clone())
}

pub fn find_store_entry(entries: &[StoreEntry], content_id: u64) -> Option<&StoreEntry> {
    entries.iter().find(|e| e.id == content_id)
}
