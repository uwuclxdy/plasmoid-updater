// SPDX-License-Identifier: MIT OR Apache-2.0
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::path::Path;

/// Extracts the component directory or file name from an installed path.
/// For paths ending with metadata.json: returns parent directory name.
/// For other files or directories: returns the last path component.
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

/// Determines the correct registry path for the installedfile element.
/// For directories: appends /metadata.json.
/// For files: returns the file path as-is.
pub(super) fn registry_installed_file_path(installed_path: &Path) -> String {
    if installed_path.is_file() {
        installed_path.to_string_lossy().to_string()
    } else {
        format!("{}/metadata.json", installed_path.to_string_lossy())
    }
}

/// Extracts date part from ISO timestamp.
pub(super) fn extract_date_from_iso(iso: &str) -> String {
    iso.split('T').next().unwrap_or(iso).to_string()
}

/// Checks if a path contains a directory name as a segment.
pub(super) fn path_matches_directory(path: &str, directory_name: &str) -> bool {
    path.split('/').any(|segment| segment == directory_name)
}
