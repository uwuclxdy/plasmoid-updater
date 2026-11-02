// SPDX-License-Identifier: GPL-3.0-or-later
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

mod manager;
mod utils;
mod xml;

pub(crate) use manager::{RegistryEntry, RegistryManager};

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write as _,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::{
    Error, Result,
    installer::{ComponentFailure, privilege},
    types::{AvailableUpdate, ComponentType, InstalledComponent},
};

/// Scans registry files to discover installed components.
/// Used for types that don't have metadata files.
///
/// With `system`, only the root-owned registries under the system data
/// directories are read, so a user-writable KNewStuff registry never names a
/// system install target.
pub(crate) fn scan_registry_components(
    component_type: ComponentType,
    system: bool,
) -> Result<Vec<InstalledComponent>> {
    if system {
        return scan_system_components(component_type, &crate::paths::system_data_dirs());
    }

    let Some(manager) = RegistryManager::for_component_type(component_type) else {
        return Ok(Vec::new());
    };

    Ok(user_components(
        component_type,
        manager.read_entries()?,
        &crate::paths::user_data_roots(),
    ))
}

/// Turns user-registry entries into components.
///
/// The user's KNewStuff registry is writable by anything running as the user,
/// so an entry whose install path lies outside the user's data directories
/// (or dodges them with `..`) is dropped: it would otherwise name a write
/// target the user does not own.
fn user_components(
    component_type: ComponentType,
    entries: Vec<RegistryEntry>,
    roots: &[PathBuf],
) -> Vec<InstalledComponent> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let path = utils::resolve_component_path(entry.installed_path);
            if !is_contained(&path, roots) {
                log::warn!(
                    target: "registry",
                    "ignoring {component_type} entry {:?}: {path:?} is outside the user data directories",
                    entry.name,
                );
                return None;
            }
            let directory_name = utils::extract_directory_name(&path)?;
            Some(InstalledComponent {
                name: entry.name,
                directory_name,
                version: entry.version,
                component_type,
                path,
                is_system: false,
                release_date: entry.release_date,
            })
        })
        .collect()
}

/// Returns whether `path` may be an install target: a plain absolute path
/// under one of `roots`, with no `..` component to escape it.
fn is_contained(path: &Path, roots: &[PathBuf]) -> bool {
    !path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
        && roots.iter().any(|root| path.starts_with(root))
}

/// Discovers system-wide components of a registry-only type.
///
/// The root-owned system KNewStuff registry is the source: each entry names the
/// install target and records the version and release date. An entry whose
/// `installedfile` leaves the system data directories is dropped, so no
/// registry entry can point a system write elsewhere. Walking the directories
/// instead would list distro packages the store never shipped, which no store
/// entry can update.
fn scan_system_components(
    component_type: ComponentType,
    roots: &[PathBuf],
) -> Result<Vec<InstalledComponent>> {
    scan_system_components_trusting(component_type, roots, crate::paths::is_trusted_system_path)
}

/// `trusted` decides which roots, `knewstuff3` directories and registry files
/// the scan may read: a root, its registry directory and the registry file must
/// each be trusted, so a registry anything running as the user can rewrite
/// never names a write target that needs root.
fn scan_system_components_trusting(
    component_type: ComponentType,
    roots: &[PathBuf],
    trusted: fn(&Path) -> bool,
) -> Result<Vec<InstalledComponent>> {
    let Some(registry_file) = component_type.registry_file() else {
        return Ok(Vec::new());
    };

    let trusted_roots: Vec<PathBuf> = roots.iter().filter(|root| trusted(root)).cloned().collect();
    let mut components = Vec::new();
    let mut seen = HashSet::new();

    for registry in trusted_registries(&trusted_roots, registry_file, trusted) {
        for entry in read_registry_file(&registry)? {
            let path = utils::resolve_component_path(entry.installed_path);
            if !is_contained(&path, &trusted_roots) {
                log::warn!(
                    target: "registry",
                    "ignoring {component_type} system entry {:?}: {path:?} is outside the system data directories",
                    entry.name,
                );
                continue;
            }
            let Some(directory_name) = utils::extract_directory_name(&path) else {
                continue;
            };
            if !seen.insert(directory_name.clone()) {
                continue;
            }
            components.push(InstalledComponent {
                name: entry.name,
                directory_name,
                version: entry.version,
                component_type,
                path,
                is_system: true,
                release_date: entry.release_date,
            });
        }
    }

    Ok(components)
}

/// Returns each trusted root's registry file for `registry_file`, in root
/// order, keeping only the ones whose `knewstuff3` directory and file are
/// trusted too. A path that fails the check is named in a warning by `trusted`.
fn trusted_registries(
    roots: &[PathBuf],
    registry_file: &str,
    trusted: fn(&Path) -> bool,
) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| {
            let dir = root.join("knewstuff3");
            let registry = dir.join(registry_file);
            (trusted(&dir) && trusted(&registry)).then_some(registry)
        })
        .collect()
}

/// Reads a registry file's entries, naming the file when its XML does not parse.
fn read_registry_file(path: &Path) -> Result<Vec<RegistryEntry>> {
    let content = fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    xml::parse_registry_entries(&content).map_err(|e| match e {
        Error::Parse { what, detail } => Error::Parse {
            what,
            detail: format!("{}: {detail}", path.display()),
        },
        other => other,
    })
}

/// Loads registry entries into a map keyed by directory name.
/// Used to look up the type and release date of installed components.
///
/// A `--system` run reads only the trusted system registries: the user's
/// registry is writable by anything running as the user, so an entry in it must
/// not decide the type or release date of a system component either.
pub(crate) fn load_registry_map(
    component_type: ComponentType,
    system: bool,
) -> Result<HashMap<String, RegistryEntry>> {
    load_registry_map_trusting(
        component_type,
        system,
        &crate::paths::knewstuff_dir(),
        &crate::paths::system_data_dirs(),
        crate::paths::is_trusted_system_path,
    )
}

/// `user_knewstuff`, `system_roots` and `trusted` decide the source: the user's
/// KNewStuff directory on a user run, a trusted registry under a trusted root
/// (its `knewstuff3` directory and file passing too) on a `--system` run.
fn load_registry_map_trusting(
    component_type: ComponentType,
    system: bool,
    user_knewstuff: &Path,
    system_roots: &[PathBuf],
    trusted: fn(&Path) -> bool,
) -> Result<HashMap<String, RegistryEntry>> {
    let Some(registry_file) = component_type.registry_file() else {
        return Ok(HashMap::new());
    };

    let registries = if system {
        let trusted_roots: Vec<PathBuf> = system_roots
            .iter()
            .filter(|root| trusted(root))
            .cloned()
            .collect();
        trusted_registries(&trusted_roots, registry_file, trusted)
    } else {
        vec![user_knewstuff.join(registry_file)]
    };

    let mut map = HashMap::new();
    for path in registries {
        let exists = path.try_exists().map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?;
        if !exists {
            continue;
        }
        for entry in read_registry_file(&path)? {
            if let Some(directory_name) = utils::extract_directory_name(&entry.installed_path) {
                map.entry(directory_name).or_insert(entry);
            }
        }
    }
    Ok(map)
}

/// Returns the filesystem path to the KNewStuff registry file for a component type.
pub(crate) fn registry_path(component_type: ComponentType) -> Option<PathBuf> {
    component_type
        .registry_file()
        .map(|f| crate::paths::knewstuff_dir().join(f))
}

/// Builds a directory_name -> content_id lookup cache from the registry files
/// that may name this mode's components.
///
/// Reads each registry file once and maps each installed directory name to the
/// store id recorded for it, eliminating the need for per-component file I/O
/// during resolution. A user run reads the user's KNewStuff directory. A
/// `--system` run reads only the trusted system registries: the user's registry
/// is writable by anything running as the user, so an entry in it must never
/// choose the store content a privileged update installs.
pub(crate) fn build_id_cache(system: bool) -> HashMap<String, u64> {
    build_id_cache_trusting(
        system,
        &crate::paths::knewstuff_dir(),
        &crate::paths::system_data_dirs(),
        crate::paths::is_trusted_system_path,
    )
}

/// `system_roots` and `trusted` decide the system source: a trusted registry
/// under a trusted root, its `knewstuff3` directory and file all passing.
fn build_id_cache_trusting(
    system: bool,
    user_knewstuff: &Path,
    system_roots: &[PathBuf],
    trusted: fn(&Path) -> bool,
) -> HashMap<String, u64> {
    let mut cache = HashMap::new();

    for &ct in ComponentType::all() {
        let Some(file) = ct.registry_file() else {
            continue;
        };
        let registries = if system {
            trusted_registries(system_roots, file, trusted)
        } else {
            vec![user_knewstuff.join(file)]
        };

        for path in registries {
            let content = match fs::read_to_string(&path) {
                Ok(content) => content,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log::warn!(target: "registry", "{}: {e}", path.display());
                    continue;
                }
            };

            // The ID cache is a lookup table, not a discovery source: a registry
            // file that does not parse still reports itself on the read path.
            let entries = match xml::parse_raw_entries(&content) {
                Ok(entries) => entries,
                Err(e) => {
                    log::warn!(target: "registry", "{}: {e}", path.display());
                    continue;
                }
            };

            for raw in entries {
                let Some(id) = raw.content_id() else {
                    continue;
                };
                let Some(installed_path) = raw.first_installed_path() else {
                    continue;
                };
                let Some(dir_name) = utils::extract_directory_name(&installed_path) else {
                    continue;
                };
                // A trusted system registry records the system installs; a user
                // registry records only what the user's own run may update.
                if system || ct.registry_only() || !privilege::is_system_path(&installed_path) {
                    cache.insert(dir_name, id);
                }
            }
        }
    }

    cache
}

/// Updates the KNewStuff registry after a successful component update, so
/// Discover sees the correct installed version.
///
/// A user component's entry goes to the user registry. A system component's
/// entry goes to the trusted system registry that names it, and nowhere when no
/// trusted system registry does: the user registry must never become a record
/// of a root-owned write target, and the next `--system` run reads only the
/// system registries.
pub(crate) fn update_registry_after_install(update: &AvailableUpdate) -> Result<()> {
    let component = &update.installed;
    let system_roots = if component.is_system {
        crate::paths::system_data_dirs()
    } else {
        Vec::new()
    };

    let Some((registry, system)) = registry_target(component, &system_roots) else {
        if component.is_system {
            log::warn!(
                target: "registry",
                "no trusted system registry records {:?}; its version is not recorded",
                component.directory_name,
            );
        } else {
            log::debug!(
                target: "registry",
                "no registry file for {}",
                component.component_type
            );
        }
        return Ok(());
    };

    update_registry_file(&registry, update, system)
}

/// Returns the registry file an update belongs to, and whether it is a system
/// one.
fn registry_target(
    component: &InstalledComponent,
    system_roots: &[PathBuf],
) -> Option<(PathBuf, bool)> {
    if !component.is_system {
        return registry_path(component.component_type).map(|path| (path, false));
    }
    let registry_file = component.component_type.registry_file()?;
    system_registry_target(
        registry_file,
        &component.directory_name,
        system_roots,
        crate::paths::is_trusted_system_path,
    )
    .map(|registry| (registry, true))
}

/// Returns the trusted system registry under `roots` that names
/// `directory_name`: the registry the component was discovered from, so the
/// next `--system` run still finds it.
fn system_registry_target(
    registry_file: &str,
    directory_name: &str,
    roots: &[PathBuf],
    trusted: fn(&Path) -> bool,
) -> Option<PathBuf> {
    let trusted_roots: Vec<PathBuf> = roots.iter().filter(|root| trusted(root)).cloned().collect();
    trusted_registries(&trusted_roots, registry_file, trusted)
        .into_iter()
        .find(|registry| {
            read_registry_file(registry).is_ok_and(|entries| {
                entries.iter().any(|entry| {
                    utils::extract_directory_name(&entry.installed_path).as_deref()
                        == Some(directory_name)
                })
            })
        })
}

/// Writes `update`'s version into `registry`, adding its entry when absent.
///
/// `system` selects the write path: a system registry sits in a root-owned
/// directory, so its rewrite runs through sudo.
fn update_registry_file(registry: &Path, update: &AvailableUpdate, system: bool) -> Result<()> {
    let component = &update.installed;
    let release_date = utils::extract_date_from_iso(&update.release_date);

    if let Some(parent) = registry.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    let original = if registry.try_exists().map_err(|e| Error::Io {
        path: registry.to_path_buf(),
        source: e,
    })? {
        Some(fs::read_to_string(registry).map_err(|e| Error::Io {
            path: registry.to_path_buf(),
            source: e,
        })?)
    } else {
        None
    };
    let content = match &original {
        Some(original) => std::borrow::Cow::Borrowed(original.as_str()),
        None => std::borrow::Cow::Owned(xml::create_empty_registry()),
    };

    let fields = xml::UpdateFields {
        directory_name: &component.directory_name,
        content_id: update.content_id,
        new_version: &update.latest_version,
        download_url: &update.download_url,
        installed_path: &component.path,
        release_date: &release_date,
    };

    if let Some(updated) = xml::update_entry(&content, &fields)? {
        write_registry(registry, &updated, original.as_deref(), system)?;
        log::debug!(
            target: "registry",
            "updated {:?} for {}",
            registry,
            component.name
        );
        return Ok(());
    }

    let entry = xml::NewEntry {
        name: &component.name,
        component_type: component.component_type,
        content_id: update.content_id,
        version: &update.latest_version,
        download_url: &update.download_url,
        installed_path: &component.path,
        release_date: &release_date,
    };
    let new_content = xml::add_entry(&content, &entry)?;
    write_registry(registry, &new_content, original.as_deref(), system)?;
    log::debug!(
        target: "registry",
        "added {} to {:?}",
        component.name,
        registry
    );
    Ok(())
}

/// Replaces `registry`'s content atomically, through sudo when it is a system
/// registry.
fn write_registry(
    registry: &Path,
    content: &str,
    expected: Option<&str>,
    system: bool,
) -> Result<()> {
    if system && !privilege::is_root() {
        write_registry_elevated(registry, content, expected)
    } else {
        write_registry_atomically(registry, content, expected)
    }
}

/// Replaces a root-owned registry through a temp file in its own directory, an
/// fsync, a permission copy and a rename, every step through sudo.
fn write_registry_elevated(registry: &Path, content: &str, expected: Option<&str>) -> Result<()> {
    let invalid_input = || Error::Io {
        path: registry.to_path_buf(),
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    };
    let dir = registry
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(invalid_input)?;
    let name = registry
        .file_name()
        .ok_or_else(invalid_input)?
        .to_string_lossy();
    let mode = match fs::metadata(registry) {
        Ok(metadata) => Some(metadata.mode() & 0o7777),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(Error::Io {
                path: registry.to_path_buf(),
                source: e,
            });
        }
    };
    let temp = dir.join(format!(".{name}.tmp-{}", std::process::id()));

    let written = write_elevated_temp(registry, dir, &temp, content, mode, expected);

    if written.is_err() {
        let _ = privilege::remove_file(&temp);
    }
    written
}

/// Fills `temp` with `content` through sudo, keeps `mode`, and renames it over
/// `registry` once the registry is still what the caller read.
fn write_elevated_temp(
    registry: &Path,
    dir: &Path,
    temp: &Path,
    content: &str,
    mode: Option<u32>,
    expected: Option<&str>,
) -> Result<()> {
    privilege::write_file_as_root(temp, content.as_bytes()).map_err(|e| elevated_error(temp, e))?;
    sync_file(temp)?;
    if let Some(mode) = mode {
        privilege::set_mode_as_root(temp, mode).map_err(|e| elevated_error(temp, e))?;
    }
    ensure_unchanged(registry, expected)?;
    privilege::replace_file_as_root(temp, registry).map_err(|e| elevated_error(registry, e))?;
    sync_file(dir)
}

/// Converts a failed privileged command into a registry error naming its path.
fn elevated_error(path: &Path, failure: ComponentFailure) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(failure.message),
    }
}

/// Flushes `path` to disk: a registry's content before the rename, its
/// directory after it.
fn sync_file(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|handle| handle.sync_all())
        .map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })
}

/// Errors unless `path` still holds `expected` (`None`: does not exist).
///
/// Discover shares no lock with this library, so a registry rewritten since the
/// caller read it is left alone instead of overwritten.
fn ensure_unchanged(path: &Path, expected: Option<&str>) -> Result<()> {
    let current = match fs::read_to_string(path) {
        Ok(current) => Some(current),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    if current.as_deref() != expected {
        return Err(Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("changed by another program during the update"),
        });
    }
    Ok(())
}

/// Replaces `path`'s content through a temp file in its own directory, an
/// fsync and a rename.
///
/// A failed or interrupted write therefore leaves the previous registry
/// intact, and a rename cannot land across filesystems. The original file's
/// permissions are preserved.
///
/// `expected` is what the caller read before building `content`; a registry
/// that no longer holds it was rewritten by another program (Discover shares no
/// lock with this library) and is left alone. Only the window between this
/// re-read and the rename stays open.
fn write_registry_atomically(path: &Path, content: &str, expected: Option<&str>) -> Result<()> {
    let invalid_input = || Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    };
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(invalid_input)?;

    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };

    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| Error::Io {
        path: dir.to_path_buf(),
        source: e,
    })?;
    tmp.write_all(content.as_bytes()).map_err(|e| Error::Io {
        path: tmp.path().to_path_buf(),
        source: e,
    })?;
    tmp.as_file().sync_all().map_err(|e| Error::Io {
        path: tmp.path().to_path_buf(),
        source: e,
    })?;
    if let Some(permissions) = permissions {
        tmp.as_file()
            .set_permissions(permissions)
            .map_err(|e| Error::Io {
                path: tmp.path().to_path_buf(),
                source: e,
            })?;
    }

    ensure_unchanged(path, expected)?;

    tmp.persist(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e.error,
    })?;

    sync_file(dir)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    #[test]
    fn a_registry_rewrite_replaces_the_file_through_a_rename_and_keeps_its_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colorschemes.knsregistry");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let inode_before = fs::metadata(&path).unwrap().ino();

        write_registry_atomically(&path, "new", Some("old")).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o640, "permissions changed");
        assert_ne!(
            metadata.ino(),
            inode_before,
            "the file was rewritten in place, so a crash can truncate it"
        );
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(entries.len(), 1, "leftover temp file: {entries:?}");
    }

    #[test]
    fn a_user_registry_entry_outside_the_user_data_directories_is_not_a_component() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let inside = root.join("color-schemes/Good.colors");
        fs::create_dir_all(inside.parent().unwrap()).unwrap();
        fs::write(&inside, "[General]\n").unwrap();
        let entries = vec![
            RegistryEntry {
                name: "Good".to_string(),
                version: "1.0".to_string(),
                installed_path: inside.clone(),
                release_date: "2026-01-01".to_string(),
            },
            RegistryEntry {
                name: "Root".to_string(),
                version: "6.6".to_string(),
                installed_path: PathBuf::from("/usr/share/color-schemes/Root.colors"),
                release_date: String::new(),
            },
            RegistryEntry {
                name: "Traversal".to_string(),
                version: "6.6".to_string(),
                installed_path: root.join("color-schemes/../../../etc/passwd"),
                release_date: String::new(),
            },
        ];

        let components = user_components(ComponentType::ColorScheme, entries, &[root]);

        assert_eq!(components.len(), 1, "{components:?}");
        assert_eq!(components[0].name, "Good");
        assert_eq!(components[0].path, inside);
        assert!(!components[0].is_system);
    }

    #[test]
    fn system_registry_components_come_from_the_system_roots_only() {
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("system");
        let scheme_dir = system.join("color-schemes");
        fs::create_dir_all(&scheme_dir).unwrap();
        let registered = scheme_dir.join("Nightfeed.colors");
        fs::write(&registered, "[General]\n").unwrap();
        // a distro package the store never shipped: on disk, in no system registry
        fs::write(scheme_dir.join("Unregistered.colors"), "[General]\n").unwrap();
        let registry_dir = system.join("knewstuff3");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::write(
            registry_dir.join("colorschemes.knsregistry"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Nightfeed</name>
    <version>3.1</version>
    <installedfile>{registered}</installedfile>
    <id>42</id>
    <releasedate>2026-03-04</releasedate>
  </stuff>
  <stuff category="112">
    <name>Ghost</name>
    <version>9.9</version>
    <installedfile>/usr/share/color-schemes/Ghost.colors</installedfile>
    <id>43</id>
  </stuff>
  <stuff category="112">
    <name>Traversal</name>
    <version>9.9</version>
    <installedfile>{system}/color-schemes/../../../etc/passwd</installedfile>
    <id>44</id>
  </stuff>
</hotnewstuffregistry>
"#,
                registered = registered.display(),
                system = system.display(),
            ),
        )
        .unwrap();

        let components = scan_system_components_trusting(
            ComponentType::ColorScheme,
            std::slice::from_ref(&system),
            |_| true,
        )
        .unwrap();

        let mut names: Vec<&str> = components.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Nightfeed"]);
        let nightfeed = components.iter().find(|c| c.name == "Nightfeed").unwrap();
        assert_eq!(nightfeed.path, registered);
        assert_eq!(nightfeed.version, "3.1");
        assert_eq!(nightfeed.release_date, "2026-03-04");
        assert!(nightfeed.is_system);
    }

    #[test]
    fn a_system_root_owned_by_the_user_is_not_a_registry_source() {
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let scheme = root.join("color-schemes/Evil.colors");
        fs::create_dir_all(scheme.parent().unwrap()).unwrap();
        fs::write(&scheme, "[General]\n").unwrap();
        let registry_dir = root.join("knewstuff3");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::write(
            registry_dir.join("colorschemes.knsregistry"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Evil</name>
    <version>9.9</version>
    <installedfile>{}</installedfile>
    <id>1</id>
  </stuff>
</hotnewstuffregistry>
"#,
                scheme.display()
            ),
        )
        .unwrap();

        let components =
            scan_system_components(ComponentType::ColorScheme, std::slice::from_ref(&root))
                .unwrap();

        assert!(components.is_empty(), "{components:?}");
    }

    #[test]
    fn a_registry_file_without_the_root_element_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colorschemes.knsregistry");
        fs::write(&path, "not a registry\n").unwrap();

        let err = read_registry_file(&path).unwrap_err();

        assert!(matches!(&err, Error::Parse { .. }), "{err}");
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_trusted_system_registry_names_a_system_components_store_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("system");
        let registry_dir = root.join("knewstuff3");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::write(
            registry_dir.join("plasmoids.knsregistry"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="705">
    <name>System Widget</name>
    <version>1.0</version>
    <installedfile>/usr/share/plasma/plasmoids/system-widget/metadata.json</installedfile>
    <id>5150</id>
  </stuff>
</hotnewstuffregistry>
"#,
        )
        .unwrap();

        let cache = build_id_cache_trusting(
            true,
            &dir.path().join("user"),
            std::slice::from_ref(&root),
            |_| true,
        );

        assert_eq!(cache.get("system-widget"), Some(&5150));
    }

    #[test]
    fn a_system_entry_map_reads_the_trusted_system_registries_only() {
        let dir = tempfile::tempdir().unwrap();
        let user_knewstuff = dir.path().join("user-knewstuff3");
        fs::create_dir_all(&user_knewstuff).unwrap();
        fs::write(
            user_knewstuff.join("ksplash.knsregistry"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="708">
    <name>Example</name>
    <version>1.0</version>
    <installedfile>/usr/share/plasma/look-and-feel/org.example.theme</installedfile>
    <id>1</id>
  </stuff>
</hotnewstuffregistry>
"#,
        )
        .unwrap();
        let root = dir.path().join("system");
        let registry_dir = root.join("knewstuff3");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::write(
            registry_dir.join("lookandfeel.knsregistry"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="722">
    <name>Example</name>
    <version>2.0</version>
    <installedfile>/usr/share/plasma/look-and-feel/org.example.theme</installedfile>
    <id>2</id>
    <releasedate>2026-05-06</releasedate>
  </stuff>
</hotnewstuffregistry>
"#,
        )
        .unwrap();

        let system_map = load_registry_map_trusting(
            ComponentType::GlobalTheme,
            true,
            &user_knewstuff,
            std::slice::from_ref(&root),
            |_| true,
        )
        .unwrap();

        assert_eq!(system_map.len(), 1, "{system_map:?}");
        let entry = system_map.get("org.example.theme").unwrap();
        assert_eq!(entry.version, "2.0");
        assert_eq!(entry.release_date, "2026-05-06");

        let user_map = load_registry_map_trusting(
            ComponentType::SplashScreen,
            false,
            &user_knewstuff,
            &[],
            |_| true,
        )
        .unwrap();

        assert_eq!(user_map.len(), 1, "{user_map:?}");
        assert_eq!(user_map.get("org.example.theme").unwrap().version, "1.0");
    }

    const SUDO_CHILD: &str = "PLASMOID_UPDATER_SUDO_TEST_CHILD";
    const SUDO_ROOT: &str = "PLASMOID_UPDATER_SUDO_TEST_ROOT";

    const SYSTEM_REGISTRY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Foreign</name>
    <version>5.5</version>
    <installedfile>/usr/share/color-schemes/Foreign.colors</installedfile>
    <id>1</id>
  </stuff>
  <stuff category="112">
    <name>Fake</name>
    <version>1.0</version>
    <installedfile>/usr/share/color-schemes/Fake.colors</installedfile>
    <id>42</id>
  </stuff>
</hotnewstuffregistry>
"#;

    fn system_scheme_update(root: &Path) -> AvailableUpdate {
        AvailableUpdate {
            installed: InstalledComponent {
                name: "Fake".to_string(),
                directory_name: "Fake.colors".to_string(),
                version: "1.0".to_string(),
                component_type: ComponentType::ColorScheme,
                path: root.join("color-schemes/Fake.colors"),
                is_system: true,
                release_date: String::new(),
            },
            content_id: 42,
            latest_version: "2.0".to_string(),
            download_url: "https://example.test/dl".to_string(),
            store_url: "https://store.kde.org/p/42".to_string(),
            release_date: "2026-02-02".to_string(),
            checksum: None,
            download_size: None,
        }
    }

    #[test]
    fn a_system_registry_update_writes_through_sudo_and_keeps_its_mode() {
        if std::env::var_os(SUDO_CHILD).is_some() {
            let root = PathBuf::from(std::env::var(SUDO_ROOT).unwrap());
            let registry = root.join("knewstuff3/colorschemes.knsregistry");
            let done = root.join("child-done");

            update_registry_file(&registry, &system_scheme_update(&root), true).unwrap();

            let log = fs::read_to_string(root.join("sudo.log")).unwrap();
            let lines: Vec<&str> = log.lines().collect();
            assert_eq!(lines.len(), 3, "{log}");
            let temp = lines[0].strip_prefix("tee ").unwrap();
            assert_eq!(
                Path::new(temp).parent(),
                registry.parent(),
                "the temp file is not in the registry's own directory: {log}"
            );
            assert_eq!(lines[1], format!("chmod 640 {temp}"), "{log}");
            assert_eq!(
                lines[2],
                format!("mv --no-target-directory -- {temp} {}", registry.display()),
                "{log}"
            );
            let written = fs::read_to_string(&registry).unwrap();
            assert!(written.contains("<version>2.0</version>"), "{written}");
            assert!(
                written.contains("<name>Foreign</name>"),
                "a foreign entry was lost: {written}"
            );
            assert_eq!(
                fs::metadata(&registry).unwrap().mode() & 0o777,
                0o640,
                "the original mode was not kept"
            );
            fs::write(done, b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let registry_dir = root.join("knewstuff3");
        fs::create_dir_all(&registry_dir).unwrap();
        let registry = registry_dir.join("colorschemes.knsregistry");
        fs::write(&registry, SYSTEM_REGISTRY).unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o640)).unwrap();

        let fake_bin = root.join("bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let sudo = fake_bin.join("sudo");
        fs::write(
            &sudo,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
                 case \"$1\" in tee) shift; exec tee \"$@\";; esac\nexec \"$@\"\n",
                log = root.join("sudo.log").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&sudo, fs::Permissions::from_mode(0o755)).unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "registry::tests::a_system_registry_update_writes_through_sudo_and_keeps_its_mode",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fake_bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env(SUDO_CHILD, "1")
            .env(SUDO_ROOT, &root)
            .output()
            .unwrap();

        assert!(
            output.status.success() && root.join("child-done").exists(),
            "child failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            root.join("child-done").exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn the_system_registry_that_names_the_component_is_its_update_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let roots = [root.join("first"), root.join("second")];
        for (root, name) in roots.iter().zip(["Other.colors", "Target.colors"]) {
            let registry_dir = root.join("knewstuff3");
            fs::create_dir_all(&registry_dir).unwrap();
            fs::write(
                registry_dir.join("colorschemes.knsregistry"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>{name}</name>
    <installedfile>/usr/share/color-schemes/{name}</installedfile>
    <id>1</id>
  </stuff>
</hotnewstuffregistry>
"#
                ),
            )
            .unwrap();
        }

        let target =
            system_registry_target("colorschemes.knsregistry", "Target.colors", &roots, |_| {
                true
            });

        assert_eq!(
            target,
            Some(roots[1].join("knewstuff3/colorschemes.knsregistry")),
            "the second root's registry names the component"
        );
    }

    #[test]
    fn a_system_registry_under_a_root_the_user_owns_is_not_an_update_target() {
        if nix::unistd::Uid::effective().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let registry_dir = root.join("knewstuff3");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::write(
            registry_dir.join("colorschemes.knsregistry"),
            SYSTEM_REGISTRY,
        )
        .unwrap();

        let target = system_registry_target(
            "colorschemes.knsregistry",
            "Fake.colors",
            std::slice::from_ref(&root),
            crate::paths::is_trusted_system_path,
        );

        assert_eq!(target, None);
    }

    #[test]
    fn a_registry_changed_during_the_update_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colorschemes.knsregistry");
        fs::write(&path, "what another writer saved\n").unwrap();

        let result = write_registry_atomically(
            &path,
            "what we are saving",
            Some("what we read before the update"),
        );

        assert!(
            result.is_err(),
            "the write overwrote a concurrent change: {}",
            fs::read_to_string(&path).unwrap()
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "what another writer saved\n"
        );
    }
}
