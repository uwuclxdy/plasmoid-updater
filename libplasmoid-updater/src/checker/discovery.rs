// SPDX-License-Identifier: GPL-3.0-or-later

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

        let exists = match path.try_exists() {
            Ok(exists) => exists,
            Err(e) => {
                log::warn!(target: "discovery", "cannot access {}: {e}", path.display());
                false
            }
        };
        if path.as_os_str().is_empty() || !exists {
            continue;
        }

        if !scanned_dirs.insert(path.clone()) {
            continue;
        }

        // For shared directories (e.g., plasma/look-and-feel), load registry
        // maps for all component types that use this path so we can assign the
        // correct type based on which registry file contains the entry.
        let shared_types = component_type.shared_path_types();
        let registry_maps: Vec<_> = shared_types
            .iter()
            .map(|&ct| Ok((ct, registry::load_registry_map(ct)?)))
            .collect::<Result<_>>()?;

        let discovered = scan_directory(&path, component_type, system, &registry_maps)?;
        components.extend(discovered);
    }

    Ok(components)
}

fn scan_directory(
    dir: &Path,
    default_type: ComponentType,
    is_system: bool,
    registry_maps: &[(
        ComponentType,
        std::collections::HashMap<String, registry::RegistryEntry>,
    )],
) -> Result<Vec<InstalledComponent>> {
    let mut components = Vec::new();

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            log::warn!(target: "discovery", "cannot scan {}: {e}", dir.display());
            return Ok(components);
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                log::warn!(
                    target: "discovery",
                    "cannot read an entry under {}: {e}",
                    dir.display()
                );
                continue;
            }
        };
        let path = entry.path();
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => continue,
            Err(e) => {
                log::warn!(target: "discovery", "cannot stat {}: {e}", path.display());
                continue;
            }
        }

        let Some(directory_name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };

        let Some(metadata) = read_metadata(&path) else {
            continue;
        };

        let name = metadata.name().unwrap_or(&directory_name).to_string();
        let version = metadata.version().unwrap_or("0.0.0").to_string();

        // Determine the correct component type by checking which registry
        // file contains this directory name. Fall back to the default type.
        let (component_type, release_date) = registry_maps
            .iter()
            .find_map(|(ct, map)| {
                map.get(&directory_name)
                    .map(|e| (*ct, e.release_date.clone()))
            })
            .unwrap_or((default_type, String::new()));

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

/// Reads a component's metadata, preferring `metadata.json` over
/// `metadata.desktop`. A file that exists but cannot be read or parsed is
/// reported, so a present component is never dropped without a trace.
pub(super) fn read_metadata(package_dir: &Path) -> Option<PackageMetadata> {
    let json_path = package_dir.join("metadata.json");
    match fs::read_to_string(&json_path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(metadata) => return Some(metadata),
            Err(e) => log::warn!(
                target: "discovery",
                "invalid {}: {e}",
                json_path.display()
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!(target: "discovery", "cannot read {}: {e}", json_path.display()),
    }
    read_metadata_desktop(package_dir)
}

fn read_metadata_desktop(package_dir: &Path) -> Option<PackageMetadata> {
    let path = package_dir.join("metadata.desktop");
    match path.try_exists() {
        Ok(false) => return None,
        Ok(true) => {}
        Err(e) => {
            log::warn!(target: "discovery", "cannot access {}: {e}", path.display());
            return None;
        }
    }
    let entry = match freedesktop_entry_parser::parse_entry(&path) {
        Ok(entry) => entry,
        Err(e) => {
            log::warn!(target: "discovery", "invalid {}: {e}", path.display());
            return None;
        }
    };
    let section = entry.section("Desktop Entry")?;

    let attr = |key: &str| section.attr(key).first().map(|s| s.to_string());

    Some(PackageMetadata {
        kplugin: Some(crate::types::KPluginInfo {
            name: attr("Name"),
            version: attr("X-KDE-PluginInfo-Version"),
            icon: attr("Icon"),
            description: attr("Comment"),
            authors: attr("X-KDE-PluginInfo-Author")
                .map(|name| vec![crate::types::KPluginAuthor { name: Some(name) }])
                .unwrap_or_default(),
        }),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::{Mutex, Once};

    use super::*;

    static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static CAPTURE: Capture = Capture;
    static INSTALL: Once = Once::new();

    /// Keeps the warnings the discovery target logs, so a test can read them.
    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            if record.level() <= log::Level::Warn && record.target() == "discovery" {
                captured().push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    /// The captured warnings. A test that fails while holding the lock poisons
    /// it, and every later log must keep working, so a poisoned lock is read
    /// through rather than propagated.
    fn captured() -> std::sync::MutexGuard<'static, Vec<String>> {
        CAPTURED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn capture_discovery_warnings() {
        INSTALL.call_once(|| {
            let _ = log::set_logger(&CAPTURE);
            log::set_max_level(log::LevelFilter::Warn);
        });
        captured().clear();
    }

    #[test]
    fn an_unstattable_entry_is_reported_and_the_scan_continues() {
        capture_discovery_warnings();
        let dir = tempfile::tempdir().unwrap();
        let dangling = dir.path().join("org.example.dangling");
        std::os::unix::fs::symlink(dir.path().join("missing-target"), &dangling).unwrap();
        let good = dir.path().join("org.example.good");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(
            good.join("metadata.json"),
            r#"{"KPlugin": {"Name": "Good", "Version": "1.0"}}"#,
        )
        .unwrap();

        let components =
            scan_directory(dir.path(), ComponentType::PlasmaWidget, false, &[]).unwrap();

        assert_eq!(components.len(), 1);
        assert_eq!(components[0].directory_name, "org.example.good");
        let warnings = captured().clone();
        assert!(
            warnings
                .iter()
                .any(|line| line.contains(&dangling.display().to_string())),
            "no warning named {}: {warnings:?}",
            dangling.display()
        );
    }
}
