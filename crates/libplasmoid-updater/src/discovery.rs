// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{collections::HashSet, fs, path::Path};

use crate::{ComponentType, InstalledComponent, PackageMetadata, Result, registry};

pub fn scan_installed_components(system: bool) -> Result<Vec<InstalledComponent>> {
    let types = if system {
        ComponentType::all()
    } else {
        ComponentType::all_user()
    };

    let mut components = Vec::new();
    let mut scanned_dirs = HashSet::new();

    for &component_type in types {
        // use registry-based discovery for types without metadata files
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

        // skip already-scanned directories to avoid duplicates
        // (e.g., GlobalTheme and SplashScreen share the same path)
        if !scanned_dirs.insert(path.clone()) {
            continue;
        }

        // load registry to get release dates
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

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(components),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let directory_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        let metadata = match read_package_metadata(&path) {
            Some(m) => m,
            None => continue,
        };

        let name = metadata.name().unwrap_or(&directory_name).to_string();
        let version = metadata.version().unwrap_or("0.0.0").to_string();

        // look up release date from registry
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

fn read_package_metadata(package_dir: &Path) -> Option<PackageMetadata> {
    read_metadata_json(package_dir).or_else(|| read_metadata_desktop(package_dir))
}

fn read_metadata_json(package_dir: &Path) -> Option<PackageMetadata> {
    let path = package_dir.join("metadata.json");
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_metadata_desktop(package_dir: &Path) -> Option<PackageMetadata> {
    let path = package_dir.join("metadata.desktop");
    let content = fs::read_to_string(&path).ok()?;

    let mut name = None;
    let mut version = None;
    let mut icon = None;
    let mut description = None;
    let mut kpackage_structure = None;

    for line in content.lines() {
        let line = line.trim();

        if let Some(val) = line.strip_prefix("Name=") {
            name = Some(val.to_string());
        } else if let Some(val) = line.strip_prefix("X-KDE-PluginInfo-Version=") {
            version = Some(val.to_string());
        } else if let Some(val) = line.strip_prefix("Icon=") {
            icon = Some(val.to_string());
        } else if let Some(val) = line.strip_prefix("Comment=") {
            description = Some(val.to_string());
        } else if let Some(val) = line.strip_prefix("X-KDE-ServiceTypes=") {
            kpackage_structure = Some(val.to_string());
        }
    }

    Some(PackageMetadata {
        kplugin: Some(crate::types::KPluginInfo {
            name,
            version,
            icon,
            description,
        }),
        kpackage_structure,
    })
}
