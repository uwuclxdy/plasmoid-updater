// SPDX-License-Identifier: GPL-3.0-or-later
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::installer::privilege;
use crate::{
    types::{ComponentType, InstalledComponent},
    {Error, Result},
};

const COLOR_SCHEME_EXTENSIONS: &[&str] = &[".colors", ".colorscheme"];
const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".svg"];

// --- Generic Recursive Directory Search ---

/// Recursively searches a directory tree for an entry matching the predicate.
///
/// The predicate is tested against each directory. If it returns `true`,
/// that directory's path is returned. Otherwise, subdirectories are searched.
fn find_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    if predicate(dir) {
        return Some(dir.to_path_buf());
    }

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && let Some(found) = find_in_dir(&path, predicate)
            {
                return Some(found);
            }
        }
    }

    None
}

/// Recursively searches for files (not directories) matching a predicate.
fn find_file_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && predicate(&path) {
                return Some(path);
            } else if path.is_dir()
                && let Some(found) = find_file_in_dir(&path, predicate)
            {
                return Some(found);
            }
        }
    }
    None
}

// --- Utility Functions ---

fn replace_destination<F>(dest: &Path, action: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    if dest.exists() {
        if dest.is_dir() {
            privilege::remove_dir_all(dest)?;
        } else {
            privilege::remove_file(dest)?;
        }
    }

    if let Some(parent) = dest.parent() {
        privilege::create_dir_all(parent)?;
    }

    action()
}

// --- Metadata ---

pub(super) fn find_package_dir(extract_dir: &Path) -> Option<PathBuf> {
    if let Some(dir) = find_in_dir(extract_dir, |d| d.join("metadata.json").exists()) {
        return Some(dir);
    }

    find_in_dir(extract_dir, |d| d.join("metadata.desktop").exists())
}

/// Patches a `metadata.json` file to update the version and KPackageStructure fields.
pub(super) fn patch_metadata(
    metadata_path: &Path,
    component_type: ComponentType,
    new_version: &str,
) -> Result<()> {
    let content = fs::read_to_string(metadata_path)?;
    let mut json: serde_json::Value =
        serde_json::from_str(&content).map_err(Error::MetadataParse)?;

    if let Some(kpackage_type) = component_type.kpackage_type() {
        json["KPackageStructure"] = serde_json::Value::String(kpackage_type.to_string());
    }

    if let Some(kplugin) = json.get_mut("KPlugin") {
        kplugin["Version"] = serde_json::Value::String(new_version.to_string());
    }

    let patched = serde_json::to_string_pretty(&json)?;
    privilege::write_file(metadata_path, patched.as_bytes())?;

    Ok(())
}

/// Patches a `metadata.desktop` file to update the `X-KDE-PluginInfo-Version` field.
pub(super) fn patch_metadata_desktop(metadata_path: &Path, new_version: &str) -> Result<()> {
    let content = fs::read_to_string(metadata_path)?;
    let line_ending = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let mut found = false;
    let patched: String = content
        .lines()
        .map(|line| {
            if line.starts_with("X-KDE-PluginInfo-Version=") {
                found = true;
                format!("X-KDE-PluginInfo-Version={new_version}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(line_ending);

    // Preserve trailing newline if original had one
    let patched = if content.ends_with('\n') && !patched.ends_with('\n') {
        patched + line_ending
    } else {
        patched
    };

    if !found {
        log::debug!(target: "patch", "no X-KDE-PluginInfo-Version field in {}", metadata_path.display());
        return Ok(());
    }

    privilege::write_file(metadata_path, patched.as_bytes())?;
    Ok(())
}

// --- kpackagetool Installation ---

/// Builds a base `kpackagetool6` command with `-t <type>`, `sudo`, and `--global` as needed.
fn kpackagetool_cmd(kpackage_type: &str, global: bool) -> std::process::Command {
    let mut cmd = if global {
        privilege::sudo_command("kpackagetool6")
    } else {
        std::process::Command::new("kpackagetool6")
    };
    cmd.arg("-t").arg(kpackage_type);
    if global {
        cmd.arg("--global");
    }
    cmd
}

/// Installs or updates a component package using `kpackagetool6`.
///
/// Tries `-u` (update) first. If that fails (e.g. stale kpackage DB entry after
/// manual deletion), removes the old entry with `-r` and retries with `-i` (install).
fn install_via_kpackagetool(
    package_dir: &Path,
    component: &InstalledComponent,
    global: bool,
) -> Result<()> {
    let kpackage_type = component
        .component_type
        .kpackage_type()
        .expect("install_via_kpackagetool called without kpackage_type");

    // Try update first — the common path
    let output = kpackagetool_cmd(kpackage_type, global)
        .arg("-u")
        .arg(package_dir)
        .output()
        .map_err(|e| Error::install(format!("failed to run kpackagetool6: {e}")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    log::debug!(
        target: "install",
        "kpackagetool6 -u failed for {}: {}",
        component.name,
        stderr.trim(),
    );

    // Remove stale DB entry (ignore failure — it may not exist)
    let remove_output = kpackagetool_cmd(kpackage_type, global)
        .arg("-r")
        .arg(&component.directory_name)
        .output();

    if let Ok(ref out) = remove_output
        && out.status.success()
    {
        log::debug!(
            target: "install",
            "removed stale kpackage entry for {}",
            component.directory_name,
        );
    }

    // Fresh install
    let output = kpackagetool_cmd(kpackage_type, global)
        .arg("-i")
        .arg(package_dir)
        .output()
        .map_err(|e| Error::install(format!("failed to run kpackagetool6: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::install(format!(
            "kpackagetool6 failed: {}",
            stderr.trim()
        )));
    }

    Ok(())
}

/// Installs or updates a component using kpackagetool, with metadata patching.
pub(super) fn install_via_kpackage(
    extract_dir: &Path,
    component: &InstalledComponent,
    new_version: &str,
) -> Result<()> {
    let package_dir = find_package_dir(extract_dir).ok_or(Error::MetadataNotFound)?;

    let metadata_json = package_dir.join("metadata.json");
    let metadata_desktop = package_dir.join("metadata.desktop");

    if metadata_json.exists()
        && let Err(e) = patch_metadata(&metadata_json, component.component_type, new_version)
    {
        log::warn!(target: "patch", "failed for {}: {e}", component.name);
    }

    if metadata_desktop.exists()
        && let Err(e) = patch_metadata_desktop(&metadata_desktop, new_version)
    {
        log::warn!(target: "patch", "failed to patch metadata.desktop for {}: {e}", component.name);
    }

    let is_global = privilege::is_system_path(&component.path);
    install_via_kpackagetool(&package_dir, component, is_global)
}

// --- Component Locators ---

/// Locates a color scheme file in an archive directory.
fn locate_color_scheme_file(dir: &Path) -> Option<PathBuf> {
    find_file_in_dir(dir, |path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                COLOR_SCHEME_EXTENSIONS
                    .iter()
                    .any(|ext| name.ends_with(ext))
            })
    })
}

/// Finds the root directory of a component within an extracted archive.
fn find_component_root_in_archive(
    extract_dir: &Path,
    component_type: ComponentType,
) -> Option<PathBuf> {
    if has_component_structure(extract_dir, component_type) {
        return Some(extract_dir.to_path_buf());
    }

    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && has_component_structure(&path, component_type) {
                return Some(path);
            }
        }
    }

    None
}

fn has_component_structure(dir: &Path, component_type: ComponentType) -> bool {
    match component_type {
        ComponentType::AuroraeDecoration => {
            dir.join("decoration.svg").exists() || dir.join("aurorae").exists()
        }
        ComponentType::GlobalTheme | ComponentType::SplashScreen => {
            dir.join("metadata.json").exists() || dir.join("metadata.desktop").exists()
        }
        ComponentType::PlasmaStyle => {
            dir.join("colors").exists()
                || dir.join("widgets").exists()
                || dir.join("metadata.desktop").exists()
        }
        ComponentType::SddmTheme => {
            dir.join("theme.conf").exists() || dir.join("Main.qml").exists()
        }
        ComponentType::KWinSwitcher => {
            dir.join("metadata.json").exists() || dir.join("contents").exists()
        }
        _ => false,
    }
}

fn find_icon_theme_dir(extract_dir: &Path) -> Option<PathBuf> {
    find_in_dir(extract_dir, |d| d.join("index.theme").exists())
}

fn find_wallpaper_source(extract_dir: &Path) -> Option<PathBuf> {
    // directory-based wallpaper (with contents/ or metadata.json)
    if let Some(dir) = find_in_dir(extract_dir, |d| {
        d.join("contents").exists() || d.join("metadata.json").exists()
    }) {
        return Some(dir);
    }

    // single-file wallpaper (image file)
    if let Ok(entries) = fs::read_dir(extract_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                let lower = name.to_lowercase();
                if IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
                    return Some(path);
                }
            }
        }
    }

    None
}

// --- Direct Installation Methods ---

/// Installs a component using direct file operations (not kpackagetool).
pub(super) fn install_direct(extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
    match component.component_type {
        ComponentType::ColorScheme => install_color_scheme(extract_dir, &component.path),
        ComponentType::IconTheme => install_icon_theme(extract_dir, &component.path),
        ComponentType::Wallpaper => install_wallpaper(extract_dir, component),
        ComponentType::AuroraeDecoration
        | ComponentType::GlobalTheme
        | ComponentType::PlasmaStyle
        | ComponentType::SplashScreen
        | ComponentType::SddmTheme => {
            install_theme_dir(extract_dir, &component.path, component.component_type)
        }
        _ => Err(Error::install(format!(
            "{} should use kpackagetool",
            component.component_type
        ))),
    }
}

fn install_color_scheme(extract_dir: &Path, dest_path: &Path) -> Result<()> {
    let color_file = locate_color_scheme_file(extract_dir)
        .ok_or_else(|| Error::install("no color scheme file found in archive"))?;

    replace_destination(dest_path, || {
        privilege::copy_file(&color_file, dest_path)?;
        log::debug!(target: "install", "copied color scheme to {}", dest_path.display());
        Ok(())
    })
}

fn install_icon_theme(extract_dir: &Path, dest_dir: &Path) -> Result<()> {
    let source_dir = find_icon_theme_dir(extract_dir)
        .ok_or_else(|| Error::install("no icon theme (index.theme) found in archive"))?;

    replace_destination(dest_dir, || {
        privilege::create_dir_all(dest_dir)?;
        privilege::copy_dir(&source_dir, dest_dir)?;
        log::debug!(target: "install", "copied icon theme to {}", dest_dir.display());
        Ok(())
    })
}

fn install_wallpaper(extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
    let source = find_wallpaper_source(extract_dir)
        .ok_or_else(|| Error::install("no wallpaper found in archive"))?;

    let dest = &component.path;

    if source.is_file() {
        replace_destination(dest, || {
            privilege::copy_file(&source, dest)?;
            log::debug!(target: "install", "copied wallpaper to {}", dest.display());
            Ok(())
        })
    } else {
        replace_destination(dest, || {
            privilege::create_dir_all(dest)?;
            privilege::copy_dir(&source, dest)?;
            log::debug!(target: "install", "copied wallpaper dir to {}", dest.display());
            Ok(())
        })
    }
}

fn install_theme_dir(
    extract_dir: &Path,
    dest_dir: &Path,
    component_type: ComponentType,
) -> Result<()> {
    let source_dir =
        find_component_root_in_archive(extract_dir, component_type).ok_or_else(|| {
            Error::install(format!(
                "no valid {component_type} structure found in archive"
            ))
        })?;

    replace_destination(dest_dir, || {
        privilege::create_dir_all(dest_dir)?;
        privilege::copy_dir(&source_dir, dest_dir)?;
        log::debug!(target: "install", "copied {} to {}", component_type, dest_dir.display());
        Ok(())
    })
}

/// Returns `true` if the path is a single-file component (e.g., color scheme file, image).
pub(super) fn is_single_file_component(path: &Path, component_type: ComponentType) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_lowercase();

    match component_type {
        ComponentType::ColorScheme => COLOR_SCHEME_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(ext)),
        ComponentType::Wallpaper => IMAGE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)),
        _ => false,
    }
}

pub(super) fn install_raw_file(downloaded: &Path, component: &InstalledComponent) -> Result<()> {
    let dest = &component.path;

    replace_destination(dest, || {
        privilege::copy_file(downloaded, dest)?;
        log::debug!(target: "install", "copied raw file to {}", dest.display());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_metadata_desktop_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.desktop");
        std::fs::write(&path, "[Desktop Entry]\r\nX-KDE-PluginInfo-Version=1.0\r\nName=Test\r\n")
            .unwrap();
        patch_metadata_desktop(&path, "2.0").unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("\r\n"), "should preserve CRLF");
        assert!(result.contains("X-KDE-PluginInfo-Version=2.0"));
        assert!(!result.contains("X-KDE-PluginInfo-Version=1.0"));
    }

    #[test]
    fn patch_metadata_desktop_preserves_lf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.desktop");
        std::fs::write(&path, "[Desktop Entry]\nX-KDE-PluginInfo-Version=1.0\nName=Test\n")
            .unwrap();
        patch_metadata_desktop(&path, "2.0").unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(!result.contains("\r\n"), "should not introduce CRLF");
        assert!(result.contains("X-KDE-PluginInfo-Version=2.0"));
    }
}
