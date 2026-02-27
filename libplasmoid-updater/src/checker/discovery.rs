// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::HashSet, fs, path::Path};

use crate::{
    Result, registry,
    types::{ComponentType, InstalledComponent, PackageMetadata},
};

/// Discovers all installed Plasmoids.
///
/// When `system` is `true`, scans system-wide directories (`/usr/share/...`);
/// otherwise scans user directories (`~/.local/share/...`).
pub(crate) fn find_installed(system: bool) -> Result<Vec<InstalledComponent>> {
    let types = if system {
        ComponentType::all()
    } else {
        ComponentType::all_user()
    };

    let mut components = Vec::new();
    let mut scanned_dirs = HashSet::new();

    for &component_type in types {
        if component_type.registry_only() {
            let registry_components = registry::scan_registry_components(component_type)?;
            components.extend(registry_components);
            continue;
        }

        let path = if system {
            component_type.system_path()
        } else {
            component_type.user_path()
        };

        if path.as_os_str().is_empty() || !path.exists() {
            continue;
        }

        if !scanned_dirs.insert(path.clone()) {
            continue;
        }

        let registry_map = registry::load_registry_map(component_type);
        let discovered = scan_directory(&path, component_type, system, &registry_map)?;
        components.extend(discovered);
    }

    Ok(components)
}

fn scan_directory(
    dir: &Path,
    component_type: ComponentType,
    is_system: bool,
    registry_map: &std::collections::HashMap<String, registry::RegistryEntry>,
) -> Result<Vec<InstalledComponent>> {
    let mut components = Vec::new();

    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(components);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(directory_name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };

        let Some(metadata) = read_metadata_json(&path).or_else(|| read_metadata_desktop(&path))
        else {
            continue;
        };

        let name = metadata.name().unwrap_or(&directory_name).to_string();
        let version = metadata.version().unwrap_or("0.0.0").to_string();

        let release_date = registry_map
            .get(&directory_name)
            .map(|e| e.release_date.clone())
            .unwrap_or_default();

        components.push(InstalledComponent {
            name,
            directory_name,
            version,
            component_type,
            path: path.clone(),
            is_system,
            release_date,
        });
    }

    Ok(components)
}

fn read_metadata_json(package_dir: &Path) -> Option<PackageMetadata> {
    let path = package_dir.join("metadata.json");
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_metadata_desktop(package_dir: &Path) -> Option<PackageMetadata> {
    let path = package_dir.join("metadata.desktop");
    let entry = freedesktop_entry_parser::parse_entry(&path).ok()?;
    let section = entry.section("Desktop Entry")?;

    let attr = |key: &str| section.attr(key).first().map(|s| s.to_string());

    Some(PackageMetadata {
        kplugin: Some(crate::types::KPluginInfo {
            name: attr("Name"),
            version: attr("X-KDE-PluginInfo-Version"),
            icon: attr("Icon"),
            description: attr("Comment"),
        }),
    })
}
