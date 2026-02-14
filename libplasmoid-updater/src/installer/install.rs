// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{ComponentType, Error, InstalledComponent, Result, backup::copy_dir_recursive};

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

// --- Installation Strategy Pattern ---

/// Strategy trait for component-type-specific installation logic.
trait InstallStrategy {
    fn install(&self, extract_dir: &Path, component: &InstalledComponent) -> Result<()>;
}

struct ColorSchemeInstaller;
struct IconThemeInstaller;
struct WallpaperInstaller;
struct ThemeDirInstaller;

impl InstallStrategy for ColorSchemeInstaller {
    fn install(&self, extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
        install_color_scheme(extract_dir, &component.path)
    }
}

impl InstallStrategy for IconThemeInstaller {
    fn install(&self, extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
        install_icon_theme(extract_dir, &component.path)
    }
}

impl InstallStrategy for WallpaperInstaller {
    fn install(&self, extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
        install_wallpaper(extract_dir, component)
    }
}

impl InstallStrategy for ThemeDirInstaller {
    fn install(&self, extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
        install_theme_dir(extract_dir, &component.path, component.component_type)
    }
}

/// Returns the appropriate installation strategy for a component type,
/// or `None` if the type should use kpackagetool instead.
fn get_install_strategy(component_type: ComponentType) -> Option<Box<dyn InstallStrategy>> {
    match component_type {
        ComponentType::ColorScheme => Some(Box::new(ColorSchemeInstaller)),
        ComponentType::IconTheme => Some(Box::new(IconThemeInstaller)),
        ComponentType::Wallpaper => Some(Box::new(WallpaperInstaller)),
        ComponentType::AuroraeDecoration
        | ComponentType::GlobalTheme
        | ComponentType::PlasmaStyle
        | ComponentType::SplashScreen
        | ComponentType::SddmTheme => Some(Box::new(ThemeDirInstaller)),
        _ => None,
    }
}

// --- Utility Functions ---

fn replace_destination<F>(dest: &Path, action: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    if dest.exists() {
        if dest.is_dir() {
            fs::remove_dir_all(dest)?;
        } else {
            fs::remove_file(dest)?;
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    action()
}

// --- Metadata ---

/// Searches for a `metadata.json` file within an extracted archive directory tree.
pub fn find_metadata_json(dir: &Path) -> Option<PathBuf> {
    find_in_dir(dir, |d| d.join("metadata.json").exists()).map(|d| d.join("metadata.json"))
}

pub(super) fn find_package_dir(extract_dir: &Path) -> Option<PathBuf> {
    if let Some(dir) = find_in_dir(extract_dir, |d| d.join("metadata.json").exists()) {
        return Some(dir);
    }

    find_in_dir(extract_dir, |d| d.join("metadata.desktop").exists())
}

/// Patches a `metadata.json` file to update the version and KPackageStructure fields.
pub fn patch_metadata(
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
    fs::write(metadata_path, patched)?;

    Ok(())
}

// --- kpackagetool Installation ---

/// Installs or updates a component package using `kpackagetool6`.
pub fn install_via_kpackagetool(package_dir: &Path, component_type: ComponentType) -> Result<()> {
    let kpackage_type = component_type
        .kpackage_type()
        .ok_or_else(|| Error::install(format!("{component_type} has no kpackage type")))?;

    let output = Command::new("kpackagetool6")
        .args(["-t", kpackage_type, "-u", &package_dir.to_string_lossy()])
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
    if metadata_json.exists()
        && let Err(e) = patch_metadata(&metadata_json, component.component_type, new_version)
    {
        log::warn!(target: "patch", "failed for {}: {e}", component.name);
    }

    install_via_kpackagetool(&package_dir, component.component_type)
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
///
/// Uses the strategy pattern to select the appropriate installation method
/// based on the component type.
pub fn install_direct(extract_dir: &Path, component: &InstalledComponent) -> Result<()> {
    let strategy = get_install_strategy(component.component_type).ok_or_else(|| {
        Error::install(format!(
            "{} should use kpackagetool",
            component.component_type
        ))
    })?;

    strategy.install(extract_dir, component)
}

fn install_color_scheme(extract_dir: &Path, dest_path: &Path) -> Result<()> {
    let color_file = locate_color_scheme_file(extract_dir)
        .ok_or_else(|| Error::install("no color scheme file found in archive"))?;

    replace_destination(dest_path, || {
        fs::copy(&color_file, dest_path)?;
        log::debug!(target: "install", "copied color scheme to {}", dest_path.display());
        Ok(())
    })
}

fn install_icon_theme(extract_dir: &Path, dest_dir: &Path) -> Result<()> {
    let source_dir = find_icon_theme_dir(extract_dir)
        .ok_or_else(|| Error::install("no icon theme (index.theme) found in archive"))?;

    replace_destination(dest_dir, || {
        fs::create_dir_all(dest_dir)?;
        copy_dir_recursive(&source_dir, dest_dir)?;
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
            fs::copy(&source, dest)?;
            log::debug!(target: "install", "copied wallpaper to {}", dest.display());
            Ok(())
        })
    } else {
        replace_destination(dest, || {
            fs::create_dir_all(dest)?;
            copy_dir_recursive(&source, dest)?;
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
        fs::create_dir_all(dest_dir)?;
        copy_dir_recursive(&source_dir, dest_dir)?;
        log::debug!(target: "install", "copied {} to {}", component_type, dest_dir.display());
        Ok(())
    })
}

/// Returns `true` if the path is a single-file component (e.g., color scheme file, image).
pub fn is_single_file_component(path: &Path, component_type: ComponentType) -> bool {
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
        fs::copy(downloaded, dest)?;
        log::debug!(target: "install", "copied raw file to {}", dest.display());
        Ok(())
    })
}
