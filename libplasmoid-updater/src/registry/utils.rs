// SPDX-License-Identifier: GPL-3.0-or-later
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::path::{Path, PathBuf};

use crate::types::ComponentType;

/// Extracts the component directory or file name from an installed path.
///
/// A path ending in `metadata.json` or `metadata.desktop` names a file inside
/// the component root, so this returns the parent directory's name. Any other
/// path names the component directly, so this returns its last component.
pub(super) fn extract_directory_name(path: &Path) -> Option<String> {
    let name = path.file_name().and_then(|n| n.to_str())?;

    if name == "metadata.json" || name == "metadata.desktop" {
        return path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
    }

    Some(name.to_string())
}

/// Resolves the installed_path from a registry entry to the actual component path.
/// KNewStuff stores metadata.json/metadata.desktop file paths; this returns the parent
/// directory for those, so InstalledComponent.path always points to the component root.
pub(super) fn resolve_component_path(path: PathBuf) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if (name == "metadata.json" || name == "metadata.desktop")
        && let Some(parent) = path.parent()
    {
        return parent.to_path_buf();
    }
    path
}

/// Determines the correct registry path for the installedfile element.
///
/// A cursor theme records `<dir>/*`: its installed files are the cursors, and
/// the theme carries no `metadata.json`.
pub(super) fn registry_installed_file_path(
    installed_path: &Path,
    component_type: ComponentType,
) -> String {
    if component_type == ComponentType::CursorTheme {
        return format!("{}/*", installed_path.to_string_lossy());
    }
    if installed_path.is_file() {
        installed_path.to_string_lossy().to_string()
    } else {
        format!("{}/metadata.json", installed_path.to_string_lossy())
    }
}

/// Extracts date part from ISO timestamp.
pub(crate) fn extract_date_from_iso(iso: &str) -> String {
    iso.split('T').next().unwrap_or(iso).to_string()
}

/// Normalizes an absolute `path` lexically: folds `.` and `..` without
/// touching the filesystem, so two spellings of one directory (`/a/b/../c`
/// and `/a/c`) compare equal without following a symlink. A `..` cannot climb
/// above the filesystem root. The registry id cache keys and the discovered
/// component paths are normalized with this, never with `canonicalize`, whose
/// symlink resolution would let a `--system` trust check compare a resolved
/// path against the original spelling.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                out.push(component.as_os_str());
            }
            std::path::Component::Normal(_) => out.push(component.as_os_str()),
        }
    }
    out
}
