// SPDX-License-Identifier: GPL-3.0-or-later
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    borrow::Cow,
    collections::BTreeSet,
    fs,
    io::{ErrorKind, Read as _},
    path::{Path, PathBuf},
};

use super::ComponentFailure;
use crate::installer::privilege;
use crate::{
    FailureKind,
    types::{ComponentType, InstalledComponent},
};

const COLOR_SCHEME_EXTENSIONS: &[&str] = &[".colors", ".colorscheme"];
const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".svg"];

/// Suffixes of the hidden sibling paths the installer writes beside a
/// component: the tree being built, and the tree being replaced.
const TEMP_NEW_SUFFIX: &str = ".plasmoid-updater-new";
const TEMP_OLD_SUFFIX: &str = ".plasmoid-updater-old";

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
            // A link is not followed: it may point anywhere, including outside
            // the extract directory this search must stay in.
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                let path = entry.path();
                if let Some(found) = find_in_dir(&path, predicate) {
                    return Some(found);
                }
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
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            // A link is never the file searched for and is never descended
            // into: the search stays inside the extract directory.
            if file_type.is_file() && predicate(&path) {
                return Some(path);
            } else if file_type.is_dir()
                && let Some(found) = find_file_in_dir(&path, predicate)
            {
                return Some(found);
            }
        }
    }
    None
}

// --- Utility Functions ---

fn temp_sibling(path: &Path, suffix: &str) -> PathBuf {
    use std::ffi::OsString;
    let name = path.file_name().unwrap_or_default();
    let mut temp_name = OsString::from(".");
    temp_name.push(name);
    temp_name.push(suffix);
    path.with_file_name(temp_name)
}

/// The component a hidden sibling belongs to, or `None` when `name` is not one.
fn installer_sibling_component(name: &str) -> Option<&str> {
    let stripped = name.strip_prefix('.')?;
    stripped
        .strip_suffix(TEMP_NEW_SUFFIX)
        .or_else(|| stripped.strip_suffix(TEMP_OLD_SUFFIX))
}

/// Returns whether `name` is one of the hidden siblings the installer writes
/// beside a component, which a directory scan must never read as a component.
pub(crate) fn is_installer_sibling(name: &str) -> bool {
    installer_sibling_component(name).is_some()
}

/// Returns whether `path` exists, without folding a failed lookup into "no":
/// an unsearchable parent is an error, not an absence.
fn path_present(path: &Path) -> Result<bool, ComponentFailure> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(ComponentFailure::io(FailureKind::Install, path, e)),
    }
}

/// Removes a hidden sibling left by an interrupted run, whatever it holds.
fn remove_leftover(path: &Path) -> Result<(), ComponentFailure> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => privilege::remove_dir_all(path),
        Ok(_) => privilege::remove_file(path),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ComponentFailure::io(FailureKind::Install, path, e)),
    }
}

/// Removes a dead copy a swap left behind, warning rather than failing: the
/// tree the caller wants is installed already, so a cleanup failure must not
/// turn a good install into a failed one, but it must not vanish either.
///
/// The caller names the log target, so a recovery cleanup reads as one.
fn remove_dead_copy(path: &Path, target: &str) {
    if let Err(e) = remove_leftover(path) {
        log::warn!(
            target: target,
            "failed to remove {}: {}",
            path.display(),
            e.message
        );
    }
}

/// Installs a single file to `dest` atomically.
///
/// Copies `src` to a hidden sibling path (`.{name}.plasmoid-updater-new`), then
/// renames it into place. On POSIX this rename is atomic and replaces any existing
/// `dest` without a window where `dest` is absent.
pub(super) fn atomic_install_file(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if let Some(parent) = dest.parent() {
        privilege::create_dir_all(parent)?;
    }
    let temp = temp_sibling(dest, TEMP_NEW_SUFFIX);
    // Clean up a leftover from a previous crash
    remove_leftover(&temp)?;
    privilege::copy_file(src, &temp)?;
    privilege::rename(&temp, dest)?;
    Ok(())
}

/// Installs a directory to `dest` without a moment where `dest` is absent
/// wherever the platform allows it.
///
/// The new tree is built in `.{name}.plasmoid-updater-new`, then exchanged with
/// an existing `dest` in one step — `renameat2(RENAME_EXCHANGE)`, or
/// `mv --exchange` under sudo for a system path, wherever the kernel, the
/// filesystem and coreutils offer it — so every reader sees the whole old tree
/// or the whole new one. Otherwise the two-rename fallback runs, which has a
/// window where `dest` is absent and puts the old tree back if its second
/// rename fails. All paths are siblings in one parent directory, so they are on
/// the same filesystem.
pub(super) fn atomic_install_dir(src: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if let Some(parent) = dest.parent() {
        privilege::create_dir_all(parent)?;
    }
    let temp_new = temp_sibling(dest, TEMP_NEW_SUFFIX);
    let temp_old = temp_sibling(dest, TEMP_OLD_SUFFIX);

    settle_hidden_siblings(dest, "install")?;
    // A leftover that resisted removal would mix into the tree this install is
    // about to build, so the install fails rather than copy over it.
    remove_leftover(&temp_new)?;

    privilege::create_dir_all(&temp_new)?;
    privilege::copy_dir(src, &temp_new)?;

    if path_present(dest)? {
        let exchanged = if !privilege::needs_sudo(dest) {
            privilege::exchange(&temp_new, dest)?
        } else if privilege::mv_supports_exchange() {
            privilege::sudo_exchange(&temp_new, dest)?
        } else {
            false
        };
        if exchanged {
            // The exchange left the replaced tree under the temp name, and a
            // file destination leaves a file there.
            remove_dead_copy(&temp_new, "install");
            return Ok(());
        }
        swap_by_renames(&temp_new, dest, &temp_old)?;
    } else {
        privilege::rename(&temp_new, dest)?;
    }

    remove_dead_copy(&temp_old, "install");
    Ok(())
}

/// The swap for a filesystem that refuses the atomic exchange: move the old
/// tree aside, then move the new one in.
fn swap_by_renames(temp_new: &Path, dest: &Path, temp_old: &Path) -> Result<(), ComponentFailure> {
    // The first rename cannot land on a non-empty directory.
    remove_leftover(temp_old)?;
    privilege::rename(dest, temp_old)?;
    if let Err(e) = privilege::rename(temp_new, dest) {
        // Put the old tree back rather than report a failure that leaves the
        // component's path empty.
        put_back(temp_old, dest);
        return Err(e);
    }
    Ok(())
}

/// Moves the old tree back after a failed swap, logging a failure to do so:
/// the component's path is empty at that point, so the record is the only
/// trace of the tree left behind.
fn put_back(temp_old: &Path, dest: &Path) {
    if let Err(e) = privilege::rename(temp_old, dest) {
        log::error!(
            target: "install",
            "failed to move {} back to {}: {}",
            temp_old.display(),
            dest.display(),
            e.message
        );
    }
}

/// Puts back every component an interrupted run stranded in a hidden sibling,
/// and drops the copies that never landed.
///
/// The caller holds the run lock, so a hidden tree seen here belongs to a run
/// that died rather than to one still working beside this one.
pub(crate) fn recover_interrupted_installs(system: bool) {
    for dir in recover_dirs(system) {
        recover_in_dir(&dir);
    }
}

/// The directories a recovery scan covers, each once: global themes and splash
/// screens share one directory, and a second scan of it would only repeat every
/// decision over the same siblings.
fn recover_dirs(system: bool) -> Vec<PathBuf> {
    let types = if system {
        ComponentType::all()
    } else {
        ComponentType::all_user()
    };

    let mut dirs: Vec<PathBuf> = Vec::new();
    for &component_type in types {
        let dir = if system {
            component_type.system_path()
        } else {
            component_type.user_path()
        };
        if dir.as_os_str().is_empty() || dirs.contains(&dir) {
            continue;
        }
        dirs.push(dir);
    }
    dirs
}

fn recover_in_dir(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut components: BTreeSet<String> = BTreeSet::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(component) = installer_sibling_component(name)
            && !component.is_empty()
        {
            components.insert(component.to_string());
        }
    }

    for component in components {
        let dest = dir.join(component);
        if let Err(e) = settle_hidden_siblings(&dest, "recover") {
            log::warn!(
                target: "recover",
                "failed to restore {}: {}",
                dest.display(),
                e.message
            );
        }
    }
}

/// Settles the hidden siblings beside `dest` from the evidence they carry.
///
/// A swap moves the replaced tree aside, then moves the new one in, so the
/// siblings say how far the last run got. Beside a present `dest`, both are
/// dead copies: whatever was meant to land did. With `dest` missing, both
/// present means the second rename never landed, a kill before it ran or a
/// failure whose put-back also failed: `-new` holds the whole tree that run
/// built — the new install, or the backup a restore had copied in — while
/// `-old` holds the tree it moved aside; finishing the swap from `-new` is the
/// recovery, where putting `-old` back would restore the failed install.
/// `-new` alone was killed mid-copy, never held the component, and goes.
/// `-old` alone, with no `dest`, is only reachable after the second rename had
/// already put the built tree in `dest` and its cleanup was killed or failed,
/// and then `dest` was removed, as an uninstall does. It goes back all the
/// same, since a tree can be the only record a run left and is never dropped
/// on a guess; the next check offers the component's update again.
fn settle_hidden_siblings(dest: &Path, target: &str) -> Result<(), ComponentFailure> {
    let temp_new = temp_sibling(dest, TEMP_NEW_SUFFIX);
    let temp_old = temp_sibling(dest, TEMP_OLD_SUFFIX);

    let (dest_present, new_present, old_present) = (
        path_present(dest)?,
        path_present(&temp_new)?,
        path_present(&temp_old)?,
    );

    if dest_present {
        remove_dead_copy(&temp_old, target);
        remove_dead_copy(&temp_new, target);
        return Ok(());
    }
    if new_present && old_present {
        privilege::rename(&temp_new, dest)?;
        remove_dead_copy(&temp_old, target);
        log::info!(
            target: target,
            "finished an interrupted swap at {}",
            dest.display()
        );
        return Ok(());
    }
    if old_present {
        privilege::rename(&temp_old, dest)?;
        log::info!(
            target: target,
            "restored {} after an interrupted update",
            dest.display()
        );
        return Ok(());
    }
    remove_dead_copy(&temp_new, target);
    Ok(())
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
) -> Result<(), ComponentFailure> {
    let content = fs::read_to_string(metadata_path)
        .map_err(|e| ComponentFailure::io(FailureKind::Install, metadata_path, e))?;
    let mut json: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
        ComponentFailure::new(
            FailureKind::Install,
            format!("failed to parse {}: {e}", metadata_path.display()),
        )
    })?;

    if let Some(kpackage_type) = component_type.kpackage_type() {
        json["KPackageStructure"] = serde_json::Value::String(kpackage_type.to_string());
    }

    if let Some(kplugin) = json.get_mut("KPlugin") {
        kplugin["Version"] = serde_json::Value::String(new_version.to_string());
    }

    let patched = serde_json::to_string_pretty(&json).map_err(|e| {
        ComponentFailure::new(
            FailureKind::Install,
            format!("failed to serialize {}: {e}", metadata_path.display()),
        )
    })?;
    privilege::write_file(metadata_path, patched.as_bytes())?;

    Ok(())
}

/// Patches a `metadata.desktop` file to update the `X-KDE-PluginInfo-Version` field.
pub(super) fn patch_metadata_desktop(
    metadata_path: &Path,
    new_version: &str,
) -> Result<(), ComponentFailure> {
    let content = fs::read_to_string(metadata_path)
        .map_err(|e| ComponentFailure::io(FailureKind::Install, metadata_path, e))?;
    let line_ending = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
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

fn install_failure(message: impl Into<String>) -> ComponentFailure {
    ComponentFailure::new(FailureKind::Install, message)
}

// --- Plugin ID Resolution ---

/// Reads the KPlugin.Id from a component's metadata.json, falling back to directory_name.
fn resolve_plugin_id(component: &InstalledComponent) -> Cow<'_, str> {
    let metadata_path = component.path.join("metadata.json");
    if let Ok(content) = fs::read_to_string(&metadata_path)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(id) = json
            .get("KPlugin")
            .and_then(|kp| kp.get("Id"))
            .and_then(|v| v.as_str())
        && !id.is_empty()
    {
        return Cow::Owned(id.to_string());
    }
    Cow::Borrowed(&component.directory_name)
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
) -> Result<(), ComponentFailure> {
    let kpackage_type = component.component_type.kpackage_type().ok_or_else(|| {
        install_failure(format!(
            "{} does not support kpackagetool6",
            component.component_type
        ))
    })?;

    // Try update first -- the common path
    let output = kpackagetool_cmd(kpackage_type, global)
        .arg("-u")
        .arg(package_dir)
        .output()
        .map_err(|e| install_failure(format!("failed to run kpackagetool6: {e}")))?;
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

    // Remove stale DB entry (ignore failure -- it may not exist)
    let plugin_id = resolve_plugin_id(component);
    let remove_output = kpackagetool_cmd(kpackage_type, global)
        .arg("-r")
        .arg(plugin_id.as_ref())
        .output();

    if let Ok(ref out) = remove_output
        && out.status.success()
    {
        log::debug!(
            target: "install",
            "removed stale kpackage entry for {}",
            plugin_id,
        );
    }

    // Fresh install
    let output = kpackagetool_cmd(kpackage_type, global)
        .arg("-i")
        .arg(package_dir)
        .output()
        .map_err(|e| install_failure(format!("failed to run kpackagetool6: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(install_failure(format!(
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
) -> Result<(), ComponentFailure> {
    let package_dir = find_package_dir(extract_dir)
        .ok_or_else(|| install_failure("no metadata.json or metadata.desktop found in package"))?;

    let metadata_json = package_dir.join("metadata.json");
    let metadata_desktop = package_dir.join("metadata.desktop");

    if metadata_json.exists()
        && let Err(e) = patch_metadata(&metadata_json, component.component_type, new_version)
    {
        log::warn!(target: "patch", "failed for {}: {}", component.name, e.message);
    }

    if metadata_desktop.exists()
        && let Err(e) = patch_metadata_desktop(&metadata_desktop, new_version)
    {
        log::warn!(
            target: "patch",
            "failed to patch metadata.desktop for {}: {}",
            component.name,
            e.message
        );
    }

    let is_global = privilege::is_system_path(&component.path);
    install_via_kpackagetool(&package_dir, component, is_global)
}

// --- Component Locators ---

/// Locates a color scheme file in an archive directory.
///
/// First tries matching by file extension (`.colors`, `.colorscheme`).
/// Falls back to content inspection, looking for KDE color scheme section
/// markers (`[Colors:`) in the first 4 KiB of each file.
fn locate_color_scheme_file(dir: &Path) -> Option<PathBuf> {
    // Try by extension first (fast path)
    if let Some(path) = find_file_in_dir(dir, |path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                COLOR_SCHEME_EXTENSIONS
                    .iter()
                    .any(|ext| name.ends_with(ext))
            })
    }) {
        return Some(path);
    }

    // Fallback: check file content for KDE color scheme markers
    find_file_in_dir(dir, |path| {
        let Ok(mut file) = fs::File::open(path) else {
            return false;
        };
        let mut buf = [0u8; 4096];
        let Ok(n) = file.read(&mut buf) else {
            return false;
        };
        std::str::from_utf8(&buf[..n]).is_ok_and(|text| text.contains("[Colors:"))
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
            // A link is never the component root: it may point outside the
            // extract directory.
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                let path = entry.path();
                if has_component_structure(&path, component_type) {
                    return Some(path);
                }
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
            // A link is never the wallpaper source: it may point outside the
            // extract directory.
            if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
                continue;
            }
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
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
pub(super) fn install_direct(
    extract_dir: &Path,
    component: &InstalledComponent,
) -> Result<(), ComponentFailure> {
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
        _ => Err(install_failure(format!(
            "{} should use kpackagetool",
            component.component_type
        ))),
    }
}

fn install_color_scheme(extract_dir: &Path, dest_path: &Path) -> Result<(), ComponentFailure> {
    let color_file = locate_color_scheme_file(extract_dir)
        .ok_or_else(|| install_failure("no color scheme file found in archive"))?;

    atomic_install_file(&color_file, dest_path)?;
    log::debug!(target: "install", "copied color scheme to {}", dest_path.display());
    Ok(())
}

fn install_icon_theme(extract_dir: &Path, dest_dir: &Path) -> Result<(), ComponentFailure> {
    let source_dir = find_icon_theme_dir(extract_dir)
        .ok_or_else(|| install_failure("no icon theme (index.theme) found in archive"))?;

    atomic_install_dir(&source_dir, dest_dir)?;
    log::debug!(target: "install", "copied icon theme to {}", dest_dir.display());
    Ok(())
}

fn install_wallpaper(
    extract_dir: &Path,
    component: &InstalledComponent,
) -> Result<(), ComponentFailure> {
    let source = find_wallpaper_source(extract_dir)
        .ok_or_else(|| install_failure("no wallpaper found in archive"))?;

    let dest = &component.path;

    if source.is_file() {
        atomic_install_file(&source, dest)?;
        log::debug!(target: "install", "copied wallpaper to {}", dest.display());
    } else {
        atomic_install_dir(&source, dest)?;
        log::debug!(target: "install", "copied wallpaper dir to {}", dest.display());
    }
    Ok(())
}

fn install_theme_dir(
    extract_dir: &Path,
    dest_dir: &Path,
    component_type: ComponentType,
) -> Result<(), ComponentFailure> {
    let source_dir =
        find_component_root_in_archive(extract_dir, component_type).ok_or_else(|| {
            install_failure(format!(
                "no valid {component_type} structure found in archive"
            ))
        })?;

    atomic_install_dir(&source_dir, dest_dir)?;
    log::debug!(target: "install", "copied {} to {}", component_type, dest_dir.display());
    Ok(())
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

pub(super) fn install_raw_file(
    downloaded: &Path,
    component: &InstalledComponent,
) -> Result<(), ComponentFailure> {
    let dest = &component.path;
    atomic_install_file(downloaded, dest)?;
    log::debug!(target: "install", "copied raw file to {}", dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::installer::testutil::{capture_logs, captured_logs};

    #[test]
    fn atomic_install_file_creates_dest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&src, b"content").unwrap();

        atomic_install_file(&src, &dest).unwrap();

        assert!(src.exists(), "src should not be consumed");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "content");
        // No temp file leftover
        let temp = temp_sibling(&dest, ".plasmoid-updater-new");
        assert!(!temp.exists());
    }

    #[test]
    fn atomic_install_file_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        atomic_install_file(&src, &dest).unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
    }

    #[test]
    fn atomic_install_file_cleans_up_leftover_temp() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&src, b"data").unwrap();
        // Simulate leftover from crashed previous run
        let leftover = temp_sibling(&dest, ".plasmoid-updater-new");
        std::fs::write(&leftover, b"garbage").unwrap();

        atomic_install_file(&src, &dest).unwrap();

        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "data");
        assert!(!leftover.exists());
    }

    #[test]
    fn atomic_install_dir_creates_dest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("file.txt"), b"hello").unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("file.txt")).unwrap(),
            "hello"
        );
        // No temp leftovers
        assert!(!temp_sibling(&dest, ".plasmoid-updater-new").exists());
        assert!(!temp_sibling(&dest, ".plasmoid-updater-old").exists());
    }

    #[test]
    fn atomic_install_dir_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old.txt"), b"old").unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert!(dest.join("new.txt").exists());
        assert!(!dest.join("old.txt").exists(), "old content must be gone");
    }

    #[test]
    fn atomic_install_dir_cleans_up_leftover_temps() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.txt"), b"x").unwrap();

        // Simulate crash leftovers
        let t_new = temp_sibling(&dest, ".plasmoid-updater-new");
        let t_old = temp_sibling(&dest, ".plasmoid-updater-old");
        std::fs::create_dir_all(&t_new).unwrap();
        std::fs::create_dir_all(&t_old).unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert!(dest.join("f.txt").exists());
        assert!(!t_new.exists());
        assert!(!t_old.exists());
    }

    #[test]
    fn atomic_install_dir_replaces_a_file_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_path");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        // The exchange needs both paths to exist, not to be of the same kind.
        std::fs::write(&dest, b"old file").unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert!(dest.join("new.txt").is_file());
        assert_eq!(std::fs::read(dest.join("new.txt")).unwrap(), b"new");
        assert!(!temp_sibling(&dest, TEMP_NEW_SUFFIX).exists());
        assert!(!temp_sibling(&dest, TEMP_OLD_SUFFIX).exists());
    }

    #[test]
    fn swap_by_renames_replaces_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest_dir");
        let temp_new = dir.path().join("new_dir");
        let temp_old = dir.path().join("old_dir");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old.txt"), b"old").unwrap();
        std::fs::create_dir_all(&temp_new).unwrap();
        std::fs::write(temp_new.join("new.txt"), b"new").unwrap();

        swap_by_renames(&temp_new, &dest, &temp_old).unwrap();

        assert!(dest.join("new.txt").is_file());
        assert!(!dest.join("old.txt").exists());
        assert!(!temp_new.exists());
        assert!(temp_old.join("old.txt").is_file());
    }

    #[test]
    fn is_installer_sibling_matches_only_the_installers_own_names() {
        assert!(is_installer_sibling(
            ".org.test.widget.plasmoid-updater-new"
        ));
        assert!(is_installer_sibling(
            ".org.test.widget.plasmoid-updater-old"
        ));
        assert!(!is_installer_sibling("org.test.widget"));
        assert!(!is_installer_sibling(".org.test.widget"));
        assert!(!is_installer_sibling("plasmoid-updater-new"));
        assert!(!is_installer_sibling(".plasmoid-updater-new-extra"));
    }

    #[test]
    fn recover_restores_a_component_stranded_in_its_old_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("org.test.widget");
        let stranded = temp_sibling(&dest, TEMP_OLD_SUFFIX);
        std::fs::create_dir_all(&stranded).unwrap();
        std::fs::write(stranded.join("metadata.json"), b"{}").unwrap();

        recover_in_dir(dir.path());

        assert!(dest.join("metadata.json").is_file());
        assert!(!stranded.exists());
    }

    #[test]
    fn recover_drops_a_new_copy_that_never_landed() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("org.test.widget");
        let partial = temp_sibling(&dest, TEMP_NEW_SUFFIX);
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("half-copied.txt"), b"half").unwrap();

        recover_in_dir(dir.path());

        assert!(!partial.exists(), "a half-copied tree became the component");
        assert!(!dest.exists());
    }

    #[test]
    fn a_directory_two_types_share_is_scanned_once() {
        let dirs = recover_dirs(false);
        let shared = ComponentType::GlobalTheme.user_path();
        assert_eq!(
            dirs.iter().filter(|dir| **dir == shared).count(),
            1,
            "the global themes and splash screens share one directory"
        );
        let unique: std::collections::BTreeSet<&PathBuf> = dirs.iter().collect();
        assert_eq!(unique.len(), dirs.len(), "a directory is scanned twice");
    }

    #[test]
    fn recover_prefers_the_new_tree_when_both_siblings_remain() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("org.test.theme");
        // What a restore killed between its two renames leaves: the backup the
        // restore had copied in, and the failed install it moved aside.
        let restored = temp_sibling(&dest, TEMP_NEW_SUFFIX);
        std::fs::create_dir_all(&restored).unwrap();
        std::fs::write(restored.join("metadata.json"), b"{}").unwrap();
        let failed = temp_sibling(&dest, TEMP_OLD_SUFFIX);
        std::fs::create_dir_all(&failed).unwrap();
        std::fs::write(failed.join("broken.txt"), b"partial").unwrap();

        recover_in_dir(dir.path());

        assert!(
            dest.join("metadata.json").is_file(),
            "the backup copy was not put in place"
        );
        assert!(
            !dest.join("broken.txt").exists(),
            "the failed install was put back"
        );
        assert!(!restored.exists());
        assert!(!failed.exists());
    }

    #[test]
    fn recover_removes_an_old_sibling_when_the_component_is_back() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("org.test.widget");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("metadata.json"), b"{}").unwrap();
        let stale_old = dir.path().join(".org.test.widget.plasmoid-updater-old");
        std::fs::create_dir_all(&stale_old).unwrap();
        std::fs::write(stale_old.join("stale.txt"), b"stale").unwrap();

        recover_in_dir(dir.path());

        assert!(
            !stale_old.exists(),
            "a dead old copy beside a present component survived"
        );
        assert!(dest.join("metadata.json").is_file());
    }

    #[test]
    fn swap_by_renames_replaces_a_leftover_old_tree() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest_dir");
        let temp_new = temp_sibling(&dest, TEMP_NEW_SUFFIX);
        let temp_old = temp_sibling(&dest, TEMP_OLD_SUFFIX);
        std::fs::create_dir_all(dest.join("old")).unwrap();
        std::fs::create_dir_all(temp_new.join("new")).unwrap();
        // What an interrupted second rename leaves: the replaced tree still
        // under its temp name, non-empty, beside the installed one.
        std::fs::create_dir_all(temp_old.join("stale")).unwrap();

        swap_by_renames(&temp_new, &dest, &temp_old).unwrap();

        assert!(dest.join("new").is_dir());
        assert!(temp_old.join("old").is_dir());
    }

    #[test]
    fn a_failed_put_back_is_logged_naming_both_paths() {
        capture_logs();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dest_dir");
        // The tree the swap moved aside is gone, so the put-back cannot land.
        let temp_old = dir.path().join("missing_old_dir");

        put_back(&temp_old, &dest);

        let logged = captured_logs();
        assert!(
            logged.iter().any(|line| line.starts_with("install: ")
                && line.contains("failed to move")
                && line.contains(&temp_old.display().to_string())
                && line.contains(&dest.display().to_string())),
            "a failed put-back was dropped: {logged:?}"
        );
    }

    #[test]
    fn two_installs_of_a_read_only_package_leave_no_sibling() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let locked = src.join("locked");
        let dest = dir.path().join("dest_dir");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("metadata.json"), b"{}").unwrap();
        // A package ships a directory with no owner write bit; the copy the
        // install makes must still be removable by the next install.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        atomic_install_dir(&src, &dest).unwrap();
        atomic_install_dir(&src, &dest).unwrap();

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(dest.join("locked").join("metadata.json").is_file());
        assert!(
            !temp_sibling(&dest, TEMP_NEW_SUFFIX).exists(),
            "a replaced tree the copy made unremovable survived the install"
        );
        assert!(!temp_sibling(&dest, TEMP_OLD_SUFFIX).exists());
    }

    #[test]
    fn install_removes_an_old_sibling_beside_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("old.txt"), b"old").unwrap();
        let stale_old = temp_sibling(&dest, TEMP_OLD_SUFFIX);
        std::fs::create_dir_all(&stale_old).unwrap();
        std::fs::write(stale_old.join("stale.txt"), b"stale").unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        assert!(dest.join("new.txt").is_file());
        assert!(
            !stale_old.exists(),
            "the dead old copy survived the install"
        );
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn resolved(program: &str) -> String {
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {program}")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Runs this test binary with only `test_name` in it, under an environment
    /// cleared down to `PATH` (led by `bin`) and the fixture's own variables.
    fn run_child(test_name: &str, bin: &Path, envs: &[(&str, &str)]) -> std::process::Output {
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", path);
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// The fake `sudo`'s argv blocks, one `Vec` per invocation.
    fn invocations(log: &str) -> Vec<Vec<String>> {
        log.split("\n\n")
            .filter(|block| !block.trim().is_empty())
            .map(|block| block.lines().map(str::to_string).collect())
            .collect()
    }

    /// The logged `mv` invocations: either a plain `sudo mv …` or the
    /// `sudo env LC_ALL=C mv …` the exchange runs.
    fn mv_invocations(log: &str) -> Vec<Vec<String>> {
        invocations(log)
            .into_iter()
            .filter(|args| {
                args.first().is_some_and(|program| program == "mv")
                    || args.get(2).is_some_and(|program| program == "mv")
            })
            .collect()
    }

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    /// The fake `mv` advertises `--exchange` whatever the host's `mv` does, so
    /// the child reliably takes the exchange branch.
    const EXCHANGE_MV: &str = r#"#!/bin/sh
case "$1" in --help) printf '  --exchange  exchange source and destination\n';; esac
exit 0
"#;

    /// The fake `sudo` logs every argv and refuses only `mv --exchange`, with
    /// the `mv` stderr the test wrote to `PU_TEST_MV_STDERR`.
    const REFUSED_EXCHANGE_SUDO: &str = r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >> "$PU_TEST_REFUSED_LOG"; done
printf '\n' >> "$PU_TEST_REFUSED_LOG"
if [ "$1" = env ] && [ "$3" = mv ] && [ "$4" = --exchange ]; then
  cat "$PU_TEST_MV_STDERR" >&2
  exit 1
fi
exit 0
"#;

    /// The child half of [`refused_exchange_falls_back_to_two_renames`]: the
    /// install itself, run where the fake `sudo` is first on `PATH`.
    fn refused_exchange_child() {
        capture_logs();
        let src = PathBuf::from(std::env::var("PU_TEST_SRC").unwrap());
        let dest = PathBuf::from(std::env::var("PU_TEST_DEST").unwrap());
        assert_eq!(
            resolved("sudo"),
            std::env::var("PU_TEST_FAKE_SUDO").unwrap(),
            "a real sudo would run against a system path"
        );

        atomic_install_dir(&src, &dest).unwrap();

        std::fs::write(
            std::env::var_os("PU_TEST_CRATE_LOG").unwrap(),
            captured_logs().join("\n"),
        )
        .unwrap();
        std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
    }

    /// Runs [`atomic_install_dir`] onto a system path whose `sudo mv --exchange`
    /// refuses with `mv_stderr`, a line measured off this box's `mv`, and
    /// asserts the swap fell back to the two renames.
    fn refused_exchange_falls_back_to_two_renames(test_name: &str, mv_stderr: &str) {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        write_script(&fake_sudo, REFUSED_EXCHANGE_SUDO);
        write_script(&bin.join("mv"), EXCHANGE_MV);

        let src = dir.path().join("src_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        let stderr = dir.path().join("mv-stderr.txt");
        std::fs::write(&stderr, mv_stderr).unwrap();
        // Never written to: the fake `sudo` runs no command, so only the
        // destination's existence is needed.
        let dest = Path::new("/usr/share");
        let new = temp_sibling(dest, TEMP_NEW_SUFFIX)
            .to_string_lossy()
            .to_string();
        let old = temp_sibling(dest, TEMP_OLD_SUFFIX)
            .to_string_lossy()
            .to_string();
        let sudo_log = dir.path().join("sudo-args.log");
        let crate_log = dir.path().join("crate.log");
        let done = dir.path().join("child-done");

        let output = run_child(
            test_name,
            &bin,
            &[
                ("PU_TEST_REFUSED_LOG", sudo_log.to_str().unwrap()),
                ("PU_TEST_MV_STDERR", stderr.to_str().unwrap()),
                ("PU_TEST_CRATE_LOG", crate_log.to_str().unwrap()),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_SRC", src.to_str().unwrap()),
                ("PU_TEST_DEST", dest.to_str().unwrap()),
                ("PU_TEST_FAKE_SUDO", fake_sudo.to_str().unwrap()),
            ],
        );
        assert!(
            output.status.success() && done.exists(),
            "child run failed: {}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        let mv_calls: Vec<Vec<String>> =
            mv_invocations(&std::fs::read_to_string(&sudo_log).unwrap());
        assert_eq!(
            mv_calls,
            vec![
                argv(&[
                    "env",
                    "LC_ALL=C",
                    "mv",
                    "--exchange",
                    "--no-target-directory",
                    new.as_str(),
                    "/usr/share"
                ]),
                argv(&["mv", "--no-target-directory", "/usr/share", old.as_str()]),
                argv(&["mv", "--no-target-directory", new.as_str(), "/usr/share"]),
            ],
            "the refused exchange did not fall back to the two-rename swap"
        );

        let crate_logs = std::fs::read_to_string(&crate_log).unwrap();
        assert!(
            crate_logs.lines().any(|line| line.starts_with("install: ")
                && line.contains("sudo mv --exchange failed")
                && line.contains(mv_stderr.trim())),
            "the fallback was not logged with its reason: {crate_logs}"
        );
    }

    /// coreutils 9.12 prints `Operation not supported` for every errno behind
    /// a filesystem that cannot exchange: EINVAL, EOPNOTSUPP and ENOSYS alike.
    #[test]
    fn a_filesystem_that_cannot_exchange_falls_back_to_two_renames() {
        const CHILD: &str = "installer::install::tests::a_filesystem_that_cannot_exchange_falls_back_to_two_renames";
        if privilege::is_root() {
            return; // root takes the plain exchange branch
        }
        if std::env::var_os("PU_TEST_REFUSED_LOG").is_some() {
            refused_exchange_child();
            return;
        }
        refused_exchange_falls_back_to_two_renames(
            CHILD,
            "mv: cannot exchange '.p-new' and 'dest-dir': Operation not supported\n",
        );
    }

    /// A destination on another filesystem answers the exchange with EXDEV.
    #[test]
    fn a_cross_device_exchange_falls_back_to_two_renames() {
        const CHILD: &str =
            "installer::install::tests::a_cross_device_exchange_falls_back_to_two_renames";
        if privilege::is_root() {
            return; // root takes the plain exchange branch
        }
        if std::env::var_os("PU_TEST_REFUSED_LOG").is_some() {
            refused_exchange_child();
            return;
        }
        refused_exchange_falls_back_to_two_renames(
            CHILD,
            "mv: cannot exchange '.plasmoid-updater-new' and '/usr/share/plasma': \
             Invalid cross-device link\n",
        );
    }

    const SYSTEM_SWAP_SUDO: &str = r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >> "$PU_TEST_SWAP_LOG"; done
printf '\n' >> "$PU_TEST_SWAP_LOG"
exit 0
"#;

    const SYSTEM_SWAP_CHILD: &str =
        "installer::install::tests::a_system_path_swap_uses_sudo_exchange";

    #[test]
    fn a_system_path_swap_uses_sudo_exchange() {
        if privilege::is_root() {
            return; // root takes the plain exchange branch
        }

        if std::env::var_os("PU_TEST_SWAP_LOG").is_some() {
            let src = PathBuf::from(std::env::var("PU_TEST_SRC").unwrap());
            let dest = PathBuf::from(std::env::var("PU_TEST_DEST").unwrap());
            assert_eq!(
                resolved("sudo"),
                std::env::var("PU_TEST_FAKE_SUDO").unwrap(),
                "a real sudo would run against a system path"
            );

            atomic_install_dir(&src, &dest).unwrap();

            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        write_script(&fake_sudo, SYSTEM_SWAP_SUDO);
        write_script(&bin.join("mv"), EXCHANGE_MV);

        let src = dir.path().join("src_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        let dest = Path::new("/usr/share");
        let new = temp_sibling(dest, TEMP_NEW_SUFFIX)
            .to_string_lossy()
            .to_string();
        let sudo_log = dir.path().join("sudo-args.log");
        let done = dir.path().join("child-done");

        let output = run_child(
            SYSTEM_SWAP_CHILD,
            &bin,
            &[
                ("PU_TEST_SWAP_LOG", sudo_log.to_str().unwrap()),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_SRC", src.to_str().unwrap()),
                ("PU_TEST_DEST", dest.to_str().unwrap()),
                ("PU_TEST_FAKE_SUDO", fake_sudo.to_str().unwrap()),
            ],
        );
        assert!(
            output.status.success() && done.exists(),
            "child run failed: {}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );

        let mv_calls: Vec<Vec<String>> =
            mv_invocations(&std::fs::read_to_string(&sudo_log).unwrap());
        assert_eq!(
            mv_calls,
            vec![argv(&[
                "env",
                "LC_ALL=C",
                "mv",
                "--exchange",
                "--no-target-directory",
                new.as_str(),
                "/usr/share"
            ])],
            "the system path swap did not use sudo mv --exchange"
        );
    }

    #[test]
    fn package_locators_do_not_follow_a_link_out_of_the_extract_dir() {
        let dir = tempfile::tempdir().unwrap();
        let extract = dir.path().join("extract");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&extract).unwrap();
        std::fs::create_dir_all(outside.join("contents")).unwrap();
        std::fs::write(outside.join("metadata.json"), b"{}").unwrap();
        std::fs::write(outside.join("index.theme"), b"[Icon Theme]").unwrap();
        std::os::unix::fs::symlink(&outside, extract.join("linked")).unwrap();

        assert!(
            find_package_dir(&extract).is_none(),
            "a link to an outside component root became the package root"
        );
        assert!(
            find_icon_theme_dir(&extract).is_none(),
            "a link to an outside icon theme became the source"
        );
        assert!(
            find_wallpaper_source(&extract).is_none(),
            "a link to an outside wallpaper became the source"
        );
        assert!(
            find_component_root_in_archive(&extract, ComponentType::GlobalTheme).is_none(),
            "a link to an outside component root became the archive root"
        );
    }

    #[test]
    fn the_color_scheme_locator_does_not_follow_a_link_out_of_the_extract_dir() {
        let dir = tempfile::tempdir().unwrap();
        let extract = dir.path().join("extract");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(extract.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Window.colors"), b"[Colors:Window]").unwrap();
        std::os::unix::fs::symlink(&outside, extract.join("real").join("linked")).unwrap();

        assert!(
            locate_color_scheme_file(&extract).is_none(),
            "a link to an outside directory became the color scheme source"
        );
    }

    #[test]
    fn recovery_cleanups_log_under_the_recovery_target() {
        if privilege::is_root() {
            return; // root removes a 0o500 directory's entries too
        }
        capture_logs();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("org.test.widget");
        std::fs::create_dir_all(&dest).unwrap();
        let stale_old = dir.path().join(".org.test.widget.plasmoid-updater-old");
        let locked = stale_old.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("kept.txt"), b"kept").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        recover_in_dir(dir.path());

        assert!(
            stale_old.exists(),
            "the fixture must keep the dead copy unremovable"
        );
        let logged = captured_logs();
        assert!(
            logged.iter().any(|line| line.starts_with("recover: ")
                && line.contains("failed to remove")
                && line.contains(&stale_old.display().to_string())),
            "the recovery cleanup logged under another target: {logged:?}"
        );

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(&stale_old);
    }

    #[test]
    fn a_failed_removal_of_the_replaced_tree_is_logged() {
        if crate::installer::privilege::is_root() {
            return; // root removes a 0o500 directory's entries too
        }
        capture_logs();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src_dir");
        let dest = dir.path().join("dest_dir");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("new.txt"), b"new").unwrap();
        let locked = dest.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("kept.txt"), b"kept").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();

        atomic_install_dir(&src, &dest).unwrap();

        let replaced = temp_sibling(&dest, TEMP_NEW_SUFFIX);
        assert!(
            replaced.exists(),
            "the fixture must keep the replaced tree unreachable"
        );
        let logged = captured_logs();
        assert!(
            logged.iter().any(|line| line.contains("failed to remove")
                && line.contains(&replaced.display().to_string())),
            "a failed removal of {} was dropped: {logged:?}",
            replaced.display()
        );

        let locked_in_replaced = replaced.join("locked");
        std::fs::set_permissions(&locked_in_replaced, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let _ = std::fs::remove_dir_all(&replaced);
    }

    #[test]
    fn patch_metadata_desktop_preserves_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("metadata.desktop");
        std::fs::write(
            &file,
            "[Desktop Entry]\r\nX-KDE-PluginInfo-Version=1.0\r\nName=Test\r\n",
        )
        .unwrap();
        patch_metadata_desktop(&file, "2.0").unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("X-KDE-PluginInfo-Version=2.0\r\n"));
        assert!(content.contains("Name=Test\r\n"));
        // Verify no bare \n exists (all should be \r\n)
        let without_cr = content.replace("\r\n", "");
        assert!(!without_cr.contains('\n'));
    }

    #[test]
    fn patch_metadata_desktop_preserves_lf() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("metadata.desktop");
        std::fs::write(
            &file,
            "[Desktop Entry]\nX-KDE-PluginInfo-Version=1.0\nName=Test\n",
        )
        .unwrap();
        patch_metadata_desktop(&file, "2.0").unwrap();
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("X-KDE-PluginInfo-Version=2.0\n"));
        assert!(!content.contains("\r\n"));
    }

    #[test]
    fn resolve_plugin_id_reads_from_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = dir.path().join("metadata.json");
        std::fs::write(
            &metadata,
            r#"{"KPlugin": {"Id": "org.kde.actual.id", "Name": "Test"}}"#,
        )
        .unwrap();

        let component = InstalledComponent {
            name: "Test".to_string(),
            directory_name: "org.kde.wrong.name".to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path: dir.path().to_path_buf(),
            is_system: false,
            release_date: String::new(),
        };

        let id = resolve_plugin_id(&component);
        assert_eq!(id.as_ref(), "org.kde.actual.id");
    }

    #[test]
    fn resolve_plugin_id_falls_back_to_directory_name() {
        let dir = tempfile::tempdir().unwrap();
        // No metadata.json
        let component = InstalledComponent {
            name: "Test".to_string(),
            directory_name: "org.kde.fallback".to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path: dir.path().to_path_buf(),
            is_system: false,
            release_date: String::new(),
        };

        let id = resolve_plugin_id(&component);
        assert_eq!(id.as_ref(), "org.kde.fallback");
    }
}
