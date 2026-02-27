// SPDX-License-Identifier: MIT OR Apache-2.0
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

mod manager;
mod utils;
mod xml;

pub(crate) use manager::{RegistryEntry, RegistryManager};

use std::{collections::HashMap, fs, path::PathBuf};

use crate::{
    Result,
    types::{AvailableUpdate, ComponentType, InstalledComponent},
};

/// Scans registry files to discover installed components.
/// Used for types that don't have metadata files.
pub(crate) fn scan_registry_components(
    component_type: ComponentType,
) -> Result<Vec<InstalledComponent>> {
    let Some(manager) = RegistryManager::for_component_type(component_type) else {
        return Ok(Vec::new());
    };

    let entries = manager.read_entries()?;

    let components = entries
        .into_iter()
        .filter_map(|entry| {
            let directory_name = utils::extract_directory_name(&entry.installed_path)?;
            Some(InstalledComponent {
                name: entry.name,
                directory_name,
                version: entry.version,
                component_type,
                path: entry.installed_path,
                is_system: false,
                release_date: entry.release_date,
            })
        })
        .collect();

    Ok(components)
}

/// Loads registry entries into a map keyed by directory name.
/// Used to look up release dates for installed components.
pub(crate) fn load_registry_map(component_type: ComponentType) -> HashMap<String, RegistryEntry> {
    RegistryManager::for_component_type(component_type)
        .map(|m| m.load_entry_map())
        .unwrap_or_default()
}

/// Returns the filesystem path to the KNewStuff registry file for a component type.
pub(crate) fn registry_path(component_type: ComponentType) -> Option<PathBuf> {
    component_type
        .registry_file()
        .map(|f| crate::paths::knewstuff_dir().join(f))
}

/// Builds a directory_name → content_id lookup cache from all registry files.
///
/// Reads each registry file once and extracts directory names and content IDs,
/// eliminating the need for per-component file I/O during resolution.
pub(crate) fn build_id_cache() -> HashMap<String, u64> {
    let mut cache = HashMap::new();
    let knewstuff = crate::paths::knewstuff_dir();

    for &ct in ComponentType::all() {
        let Some(file) = ct.registry_file() else {
            continue;
        };
        let path = knewstuff.join(file);
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };

        for raw in xml::parse_raw_entries(&content) {
            let Some(id) = raw.content_id() else {
                continue;
            };
            if let Some(installed_path) = raw.first_installed_path()
                && let Some(dir_name) = utils::extract_directory_name(&installed_path)
            {
                cache.insert(dir_name, id);
            }
        }
    }

    cache
}

/// Updates the KNS registry after a successful component update.
/// This ensures Discover sees the correct installed version.
/// If the entry doesn't exist, it creates a new one.
pub(crate) fn update_registry_after_install(update: &AvailableUpdate) -> Result<()> {
    let component = &update.installed;

    let Some(reg_path) = registry_path(component.component_type) else {
        log::debug!(
            target: "registry",
            "no registry file for {}",
            component.component_type
        );
        return Ok(());
    };

    let release_date = utils::extract_date_from_iso(&update.release_date);

    if let Some(parent) = reg_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let content = if reg_path.exists() {
        fs::read_to_string(&reg_path)?
    } else {
        xml::create_empty_registry()
    };

    let fields = xml::UpdateFields {
        directory_name: &component.directory_name,
        content_id: update.content_id,
        new_version: &update.latest_version,
        download_url: &update.download_url,
        installed_path: &component.path,
        release_date: &release_date,
    };

    let updated = xml::update_entry(&content, &fields)?;

    if let Some(new_content) = updated {
        fs::write(&reg_path, new_content)?;
        log::debug!(
            target: "registry",
            "updated {} for {}",
            reg_path.display(),
            component.name
        );
    } else {
        let entry = xml::NewEntry {
            name: &component.name,
            component_type: component.component_type,
            content_id: update.content_id,
            version: &update.latest_version,
            download_url: &update.download_url,
            installed_path: &component.path,
            release_date: &release_date,
        };
        let new_content = xml::add_entry(&content, &entry);
        fs::write(&reg_path, new_content)?;
        log::debug!(
            target: "registry",
            "added {} to {}",
            component.name,
            reg_path.display()
        );
    }

    Ok(())
}
