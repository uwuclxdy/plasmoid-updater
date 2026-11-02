// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

use crate::{
    Error, Result, registry,
    types::{ComponentType, InstalledComponent, PackageMetadata},
};

/// Discovers all installed Plasmoids.
///
/// When `system` is `true`, scans system-wide directories (`/usr/share/...`);
/// otherwise scans user directories (`~/.local/share/...`).
///
/// One registry file that does not parse is logged and skipped: it must not
/// hide the components of every other type.
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
            match registry::scan_registry_components(component_type, system) {
                Ok(found) => components.extend(found),
                Err(e @ Error::Parse { .. }) => {
                    log::error!(target: "registry", "skipping {component_type}: {e}");
                }
                Err(e) => return Err(e),
            }
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
        // correct type based on which registry file contains the entry. A
        // `--system` scan reads only the trusted system registries.
        let shared_types = component_type.shared_path_types();
        let registry_maps: Vec<_> = shared_types
            .iter()
            .map(|&ct| match registry::load_registry_map(ct, system) {
                Ok(map) => Ok((ct, map)),
                Err(e @ Error::Parse { .. }) => {
                    log::error!(target: "registry", "skipping {ct} entries: {e}");
                    Ok((ct, HashMap::new()))
                }
                Err(e) => Err(e),
            })
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

        // The installer's in-progress siblings hold the same metadata as the
        // component they belong to; reading them lists one install twice.
        if crate::installer::is_installer_sibling(&directory_name) {
            continue;
        }

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
/// reported, so a present component is never dropped without a trace. Only a
/// regular file is opened: a FIFO, socket or device named `metadata.json` would
/// otherwise wait for a writer that never comes.
pub(crate) fn read_metadata(package_dir: &Path) -> Option<PackageMetadata> {
    let json_path = package_dir.join("metadata.json");
    if !crate::installer::is_regular_file(&json_path) {
        match json_path.symlink_metadata() {
            Ok(_) => log::warn!(
                target: "discovery",
                "ignoring {}: not a regular file",
                json_path.display()
            ),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                log::warn!(target: "discovery", "cannot read {}: {e}", json_path.display());
            }
            Err(_) => {}
        }
        return read_metadata_desktop(package_dir);
    }
    if !crate::installer::metadata_size_ok(&json_path) {
        log::warn!(
            target: "discovery",
            "ignoring {}: larger than the {}-byte metadata cap",
            json_path.display(),
            crate::installer::MAX_METADATA_BYTES
        );
        return read_metadata_desktop(package_dir);
    }
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
    if !crate::installer::is_regular_file(&path) {
        log::warn!(
            target: "discovery",
            "ignoring {}: not a regular file",
            path.display()
        );
        return None;
    }
    if !crate::installer::metadata_size_ok(&path) {
        log::warn!(
            target: "discovery",
            "ignoring {}: larger than the {}-byte metadata cap",
            path.display(),
            crate::installer::MAX_METADATA_BYTES
        );
        return None;
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

    /// An unsearchable directory stops `metadata.json` from being statted at
    /// all; the scan has to name the path it could not reach.
    #[test]
    fn read_metadata_warns_when_metadata_json_cannot_be_statted() {
        use std::os::unix::fs::PermissionsExt as _;

        if crate::installer::privilege::is_root() {
            return; // root searches a directory whatever its mode
        }
        capture_discovery_warnings();
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let metadata = read_metadata(&blocked);

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            metadata.is_none(),
            "an unreadable package reported metadata"
        );
        let warnings = captured().clone();
        let json = blocked.join("metadata.json");
        assert!(
            warnings
                .iter()
                .any(|line| line.contains(&json.display().to_string())
                    && line.contains("cannot read")),
            "no warning named {}: {warnings:?}",
            json.display()
        );
    }

    #[test]
    fn read_metadata_skips_a_metadata_json_past_the_cap() {
        capture_discovery_warnings();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"KPlugin": {{"Name": "Huge"}}, "Pad": "{}"}}"#,
                "x".repeat(2 * 1024 * 1024)
            ),
        )
        .unwrap();

        let metadata = read_metadata(dir.path());

        assert!(metadata.is_none(), "a capped metadata.json was read");
        let warnings = captured().clone();
        assert!(
            warnings
                .iter()
                .any(|line| line.contains(&path.display().to_string()) && line.contains("cap")),
            "no warning named the capped file: {warnings:?}"
        );
    }

    #[test]
    fn read_metadata_does_not_block_on_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(dir.path().join("metadata.json"))
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo failed");
        let package = dir.path().to_path_buf();

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(read_metadata(&package));
        });
        let metadata = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read_metadata blocked on a FIFO for five seconds");

        assert!(metadata.is_none(), "a FIFO was read as metadata");
    }
}
