// SPDX-License-Identifier: GPL-3.0-or-later
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    io::{ErrorKind, Read},
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
};

use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};

use super::ComponentFailure;
use crate::installer::privilege;
use crate::{
    FailureKind,
    types::{ComponentType, InstalledComponent},
};

const COLOR_SCHEME_EXTENSIONS: &[&str] = &[".colors", ".colorscheme"];
const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".svg"];

/// The reason a download in no enabled image format cannot be named: only a
/// crate that turns on more of `imagesize`'s formats can reach this.
const UNSUPPORTED_IMAGE: &str = "not a PNG, JPEG or WebP image";

/// Suffixes of the hidden sibling paths the installer writes beside a
/// component: the tree being built, and the tree being replaced.
const TEMP_NEW_SUFFIX: &str = ".plasmoid-updater-new";
const TEMP_OLD_SUFFIX: &str = ".plasmoid-updater-old";

// --- Generic Recursive Directory Search ---

/// The deepest directory level a package search descends into. No KDE Store
/// package nests its root this far down, and an unbounded walk over a hostile
/// archive could overflow the stack.
const MAX_SEARCH_DEPTH: usize = 8;

/// Searches a directory tree breadth-first for the first directory matching the
/// predicate, so a shallow root always beats a deeper one.
fn find_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    let mut queue = std::collections::VecDeque::from([(dir.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if predicate(&dir) {
            return Some(dir);
        }
        if depth >= MAX_SEARCH_DEPTH {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                // A link is not followed: it may point anywhere, including outside
                // the extract directory this search must stay in.
                if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                    queue.push_back((entry.path(), depth + 1));
                }
            }
        }
    }

    None
}

/// Searches a directory tree breadth-first for every directory matching the
/// predicate. A matched directory is not descended into: its children are part
/// of the match, not further matches.
fn find_all_in_dir<F>(dir: &Path, predicate: F) -> Vec<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([(dir.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if predicate(&dir) {
            found.push(dir);
            continue;
        }
        if depth >= MAX_SEARCH_DEPTH {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                // A link is not followed: it may point anywhere, including outside
                // the extract directory this search must stay in.
                if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                    queue.push_back((entry.path(), depth + 1));
                }
            }
        }
    }
    found
}

/// Searches breadth-first for a file (not a directory) matching the predicate.
fn find_file_in_dir<F>(dir: &Path, predicate: F) -> Option<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    let mut queue = std::collections::VecDeque::from([(dir.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            // A link is never the file searched for and is never descended
            // into: the search stays inside the extract directory.
            if file_type.is_file() && predicate(&path) {
                return Some(path);
            } else if file_type.is_dir() && depth < MAX_SEARCH_DEPTH {
                queue.push_back((path, depth + 1));
            }
        }
    }
    None
}

/// Whether `path` is a regular file, checked without following the final path
/// component: a FIFO, socket or device must never be opened as data, and
/// opening a FIFO for reading waits for a writer that may never come.
pub(crate) fn is_regular_file(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_file())
}

/// Whether `path` is a directory, checked without following it.
fn is_directory(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_dir())
}

/// The largest metadata file the installer or discovery reads whole.
///
/// A sparse archive member can declare gigabytes while the archive itself
/// stays small, so the extraction cap does not bound what a metadata read
/// pulls into memory.
pub(crate) const MAX_METADATA_BYTES: u64 = 1024 * 1024;

/// Whether a metadata file is small enough to read whole; a file whose size
/// cannot be read is not, and the caller reports it.
pub(crate) fn metadata_size_ok(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.len() <= MAX_METADATA_BYTES)
}

/// Reads a metadata file the installer owns, refusing one past the cap.
fn read_metadata_text(path: &Path) -> Result<String, ComponentFailure> {
    let size = path
        .symlink_metadata()
        .map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))?
        .len();
    if size > MAX_METADATA_BYTES {
        return Err(install_failure(format!(
            "{} is larger than the {MAX_METADATA_BYTES}-byte metadata cap",
            path.display()
        )));
    }
    fs::read_to_string(path).map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))
}

/// Reads a metadata file for a read-only answer, skipping one past the cap
/// with a warning: such a caller has no failure row to report it on.
fn read_metadata_for_query(path: &Path, target: &str) -> Option<String> {
    if !metadata_size_ok(path) {
        log::warn!(
            target: target,
            "ignoring {}: larger than the {MAX_METADATA_BYTES}-byte metadata cap",
            path.display()
        );
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Drops a leading UTF-8 byte-order mark, which Qt accepts and serde rejects.
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

// --- Utility Functions ---

fn temp_sibling(path: &Path, suffix: &str) -> PathBuf {
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
/// beside a component (a tree in flight, or an install record), which a scan
/// must never read as a component.
pub(crate) fn is_installer_sibling(name: &str) -> bool {
    installer_sibling_component(name).is_some() || super::record::is_record_name(name)
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
pub(super) fn recover_dirs(system: bool) -> Vec<PathBuf> {
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

/// The components a directory's hidden siblings name, once each.
fn sibling_component_names(dir: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return names;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(component) = installer_sibling_component(name)
            && !component.is_empty()
        {
            names.insert(component.to_string());
        }
    }
    names
}

/// Returns whether [`recover_interrupted_installs`] would find a hidden sibling
/// to settle. It walks the same directories and names the repair walks, so the
/// two cannot disagree, and it only reads.
pub(crate) fn has_interrupted_installs(system: bool) -> bool {
    recover_dirs(system)
        .iter()
        .any(|dir| !sibling_component_names(dir).is_empty())
}

fn recover_in_dir(dir: &Path) {
    for component in sibling_component_names(dir) {
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

/// The display name and install path of each component a killed run stranded
/// in its `.plasmoid-updater-old` sibling with no visible component beside it.
/// The install path is where the repair puts the component back.
///
/// A `-new` sibling alone is a half-copy that never held the component, and one
/// beside its component is a dead copy: neither is a component a check names.
pub(crate) fn stranded_components(system: bool) -> Vec<(String, PathBuf)> {
    recover_dirs(system)
        .iter()
        .flat_map(|dir| stranded_in_dir(dir))
        .collect()
}

/// The stranded components of one directory, each with its display name and
/// install path.
fn stranded_in_dir(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut stranded = Vec::new();
    for component in sibling_component_names(dir) {
        let dest = dir.join(&component);
        // An unreadable path is not a proven absence: reporting it would also
        // need a write to repair it.
        if path_present(&dest).unwrap_or(true) {
            continue;
        }
        let old = temp_sibling(&dest, TEMP_OLD_SUFFIX);
        if path_present(&old).unwrap_or(false) {
            stranded.push((stranded_display_name(&old, &component), dest));
        }
    }
    stranded
}

/// A stranded tree's display name from its metadata, or its directory name when
/// the metadata is unreadable.
fn stranded_display_name(tree: &Path, directory_name: &str) -> String {
    crate::checker::read_metadata(tree)
        .and_then(|metadata| metadata.name().map(str::to_string))
        .unwrap_or_else(|| directory_name.to_string())
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

/// The root of a package in an extract directory: its shallowest directory
/// holding either metadata file, so a root `metadata.desktop` beats a nested
/// `contents/config/metadata.json`.
pub(super) fn find_package_dir(extract_dir: &Path) -> Option<PathBuf> {
    find_in_dir(extract_dir, |dir| {
        is_regular_file(&dir.join("metadata.json"))
            || is_regular_file(&dir.join("metadata.desktop"))
    })
}

/// Patches a `metadata.json` file to update the version and KPackageStructure fields.
pub(super) fn patch_metadata(
    metadata_path: &Path,
    component_type: ComponentType,
    new_version: &str,
) -> Result<(), ComponentFailure> {
    if !is_regular_file(metadata_path) {
        return Err(install_failure(format!(
            "{} is not a regular file",
            metadata_path.display()
        )));
    }
    let content = read_metadata_text(metadata_path)?;
    let mut json: serde_json::Value = serde_json::from_str(strip_bom(&content)).map_err(|e| {
        ComponentFailure::new(
            FailureKind::Install,
            format!("failed to parse {}: {e}", metadata_path.display()),
        )
    })?;

    let Some(object) = json.as_object_mut() else {
        return Err(install_failure(format!(
            "{} is not a JSON object",
            metadata_path.display()
        )));
    };

    if let Some(kpackage_type) = component_type.kpackage_type() {
        object.insert(
            "KPackageStructure".to_string(),
            serde_json::Value::String(kpackage_type.to_string()),
        );
    }

    // A KPlugin that is not an object is left alone rather than overwritten:
    // the file still installs, and the version check reports the mismatch.
    if let Some(kplugin) = object.get_mut("KPlugin") {
        match kplugin.as_object_mut() {
            Some(kplugin) => {
                kplugin.insert(
                    "Version".to_string(),
                    serde_json::Value::String(new_version.to_string()),
                );
            }
            None => log::warn!(
                target: "patch",
                "{}: KPlugin is not an object",
                metadata_path.display()
            ),
        }
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
    if !is_regular_file(metadata_path) {
        return Err(install_failure(format!(
            "{} is not a regular file",
            metadata_path.display()
        )));
    }
    let content = read_metadata_text(metadata_path)?;
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

/// The non-empty plugin id a package records, from `metadata.json`
/// `KPlugin.Id` when it carries one and from `metadata.desktop`
/// `X-KDE-PluginInfo-Name` otherwise.
///
/// Only a regular metadata file is read, and a `KPlugin` that is not an object
/// carries no id: either way the answer falls through to the next source.
pub(super) fn plugin_id(package_dir: &Path) -> Option<String> {
    json_plugin_id(package_dir).or_else(|| desktop_plugin_id(package_dir))
}

/// The non-empty `KPlugin.Id` a package's `metadata.json` records.
fn json_plugin_id(package_dir: &Path) -> Option<String> {
    let metadata_path = package_dir.join("metadata.json");
    if !is_regular_file(&metadata_path) {
        return None;
    }
    let content = read_metadata_for_query(&metadata_path, "install")?;
    let json: serde_json::Value = serde_json::from_str(strip_bom(&content)).ok()?;
    json.get("KPlugin")?
        .get("Id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// The non-empty `X-KDE-PluginInfo-Name` a package's `metadata.desktop`
/// records.
fn desktop_plugin_id(package_dir: &Path) -> Option<String> {
    let metadata_path = package_dir.join("metadata.desktop");
    if !is_regular_file(&metadata_path) {
        return None;
    }
    let content = read_metadata_for_query(&metadata_path, "install")?;
    desktop_entry_plugin_id(&content)
}

/// The id a `metadata.desktop` body records in its `[Desktop Entry]` group.
///
/// KConfig trims the key and the value and keeps the last duplicate, so a
/// raw prefix scan over every group reads a different id than KDE does.
fn desktop_entry_plugin_id(content: &str) -> Option<String> {
    let entry = freedesktop_entry_parser::Entry::parse(content.as_bytes()).ok()?;
    let section = entry.section("Desktop Entry")?;
    section
        .attr("X-KDE-PluginInfo-Name")
        .last()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Refuses a package root whose metadata entries are not regular files.
///
/// kpackagetool6 follows a symlinked `metadata.json` and installs whatever id
/// the link target names, and opening a FIFO waits for a writer that may never
/// come while the run holds the update lock. Both are refused before anything
/// reads or copies the package.
pub(super) fn verify_package_metadata(package_dir: &Path) -> Result<(), ComponentFailure> {
    for name in ["metadata.json", "metadata.desktop"] {
        let path = package_dir.join(name);
        match path.symlink_metadata() {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(install_failure(format!(
                    "{} is not a regular file",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(ComponentFailure::io(FailureKind::Install, &path, e)),
        }
    }
    Ok(())
}

/// Refuses the package root `find_package_dir` returns when its metadata is
/// not a regular file; a tree with no package root is left to the install step.
pub(super) fn verify_extracted_package(extract_dir: &Path) -> Result<(), ComponentFailure> {
    match find_package_dir(extract_dir) {
        Some(package_dir) => verify_package_metadata(&package_dir),
        None => Ok(()),
    }
}

/// Refuses an extracted package holding an entry the installer would follow
/// out of its own tree.
///
/// kpackagetool6 dereferences every symlink in the package, so an absolute
/// link, or a relative one whose target leaves the package root, copies a file
/// the store never shipped; with `--system` that copy runs as root. A relative
/// link's text is not the path it resolves to: a link that walks through
/// another link cancels a component the real path does not, so the link is
/// resolved on the filesystem and must stay under the canonical root. A link
/// that cannot be resolved (dangling, looping) is refused the same way.
///
/// A kpackagetool package refuses a link that resolves to a directory as well:
/// the tool copies it recursively and follows it again on every level, so a
/// link to an ancestor multiplies the tree. A direct copy recreates a link
/// without following it, so it keeps a link to a directory inside the root; an
/// icon theme ships those.
///
/// The walk keeps its own stack, so a deep tree cannot overflow the stack, and
/// extraction has already bounded the number of entries.
pub(super) fn verify_package_tree(
    root: &Path,
    component_type: ComponentType,
) -> Result<(), ComponentFailure> {
    let canonical_root =
        fs::canonicalize(root).map_err(|e| ComponentFailure::io(FailureKind::Install, root, e))?;
    let refuses_directory_links = component_type.kpackage_type().is_some();
    // Reported once the whole tree is walked: a link that leaves the root is
    // the worse finding, and the chain that reaches outside runs through a
    // directory link, so its refusal must not hide the containment one.
    let mut directory_link: Option<(PathBuf, PathBuf)> = None;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            fs::read_dir(&dir).map_err(|e| ComponentFailure::io(FailureKind::Install, &dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| ComponentFailure::io(FailureKind::Install, &dir, e))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| ComponentFailure::io(FailureKind::Install, &path, e))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|e| ComponentFailure::io(FailureKind::Install, &path, e))?;
                if target.is_absolute() {
                    return Err(install_failure(format!(
                        "{} is an absolute symlink, which the install would follow",
                        path.display()
                    )));
                }
                if !link_target_stays_inside(&dir, &target, root) {
                    return Err(install_failure(format!(
                        "{} points outside the package root: {}",
                        path.display(),
                        target.display()
                    )));
                }
                let resolved = fs::canonicalize(&path).map_err(|e| {
                    install_failure(format!(
                        "{} points at a target that cannot be resolved: {e}",
                        path.display()
                    ))
                })?;
                if !resolved.starts_with(&canonical_root) {
                    return Err(install_failure(format!(
                        "{} points outside the package root: {}",
                        path.display(),
                        target.display()
                    )));
                }
                if refuses_directory_links && resolved.is_dir() && directory_link.is_none() {
                    directory_link = Some((path, resolved));
                }
            } else if file_type.is_dir() {
                pending.push(path);
            } else if !file_type.is_file() {
                return Err(install_failure(format!(
                    "{} is not a regular file, directory or symlink",
                    path.display()
                )));
            }
        }
    }
    if let Some((link, resolved)) = directory_link {
        return Err(install_failure(format!(
            "{} resolves to the directory {}, which kpackagetool6 copies recursively",
            link.display(),
            resolved.display()
        )));
    }
    Ok(())
}

/// Whether `target`, resolved as text from `dir`, stays inside `root`.
///
/// This is the cheap first pass; a link whose text stays inside can still
/// resolve outside, which [`verify_package_tree`] then settles on disk.
fn link_target_stays_inside(dir: &Path, target: &Path, root: &Path) -> bool {
    let Ok(relative) = dir.strip_prefix(root) else {
        return false;
    };
    let mut resolved = PathBuf::new();
    for component in relative.components().chain(target.components()) {
        match component {
            std::path::Component::Normal(part) => resolved.push(part),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !resolved.pop() {
                    return false;
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return false,
        }
    }
    true
}

/// Refuses a store package whose plugin id names a different widget than the
/// installed component, before any install step runs.
///
/// A type whose installed side carries no id is not a widget and skips the
/// check. Once the installed side has one, a package that yields none (no
/// metadata, unreadable, unparsable, no id key) is refused as well: skipping
/// the comparison there is what lets a package install under another id.
pub(super) fn verify_package_plugin_id(
    extract_dir: &Path,
    component: &InstalledComponent,
) -> Result<(), ComponentFailure> {
    let Some(installed_id) = plugin_id(&component.path) else {
        return Ok(());
    };
    let package_id = find_package_dir(extract_dir).and_then(|dir| plugin_id(&dir));
    let Some(package_id) = package_id else {
        return Err(install_failure(format!(
            "store entry is a different widget: installed {installed_id:?}, the package records no plugin id"
        )));
    };
    if package_id != installed_id {
        return Err(install_failure(format!(
            "store entry is a different widget: installed {installed_id:?}, package {package_id:?}"
        )));
    }
    Ok(())
}

// --- kpackagetool Installation ---

/// How long a `kpackagetool6` invocation may run before it is killed: a tool
/// that blocks would otherwise hold the update lock for as long as it waits.
const KPACKAGETOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// How long a killed child and its process group get to exit before they are
/// killed outright, and how long a pipe reader or the group probe may take to
/// answer: the bound the timeout path and the privileged command helpers give
/// every pipe they wait on.
pub(super) const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the child gets to be reaped after the `SIGKILL`. A child the
/// signal cannot reach (a `sudo` the user may not signal) lives as long as its
/// command does, so waiting for it would hold the run and its lock until the
/// tool exits.
pub(super) const REAP_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

fn kpackagetool_timeout() -> std::time::Duration {
    // A unit test cannot wait out the production deadline, so it may shorten it.
    #[cfg(test)]
    {
        if let Some(millis) = std::env::var("PU_TEST_KPACKAGETOOL_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            return std::time::Duration::from_millis(millis);
        }
    }
    KPACKAGETOOL_TIMEOUT
}

/// Builds a base `kpackagetool6` command with `-t <type>`, `sudo`, and
/// `--global` as needed, with the route it runs by.
///
/// The not-installed answer this module matches is English text and
/// kpackagetool prints it in the locale it inherits, so every call runs in the
/// C locale. `sudo` resets the environment, so there the locale goes through
/// `env`, which `sudo` passes on to the tool.
fn kpackagetool_cmd(kpackage_type: &str, global: bool) -> (std::process::Command, Route) {
    let (mut cmd, route) = if global && !privilege::is_root() {
        let mut cmd = privilege::sudo_command("env");
        cmd.args(["LC_ALL=C", "LANGUAGE=C", "kpackagetool6"]);
        (cmd, Route::Elevated)
    } else {
        let mut cmd = std::process::Command::new("kpackagetool6");
        cmd.env("LC_ALL", "C").env("LANGUAGE", "C");
        (cmd, Route::Direct)
    };
    cmd.arg("-t").arg(kpackage_type);
    if global {
        cmd.arg("--global");
    }
    (cmd, route)
}

/// How a timed-out command's child relates to the tool it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// The child is the tool, running as this process's user.
    Direct,
    /// The child is the elevation command, which runs the tool as root and
    /// may put it in a session of its own, out of the child's process group.
    Elevated,
}

/// When the timed-out child was reaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reaped {
    /// On its own or within the `SIGTERM` grace, before any `SIGKILL`.
    BeforeKill,
    /// Only after the `SIGKILL`.
    AfterKill,
    /// Not within [`REAP_BOUND`] after the `SIGKILL`.
    Never,
    /// Waiting for it failed.
    WaitFailed,
}

/// What signal 0 to the child's process group answered (kill(2)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    /// `ESRCH`: no process is left in the group.
    Empty,
    /// The signal could be sent: a process is left.
    Alive,
    /// `EPERM`: a process is left that this user may not signal.
    Denied,
    /// Any other error, which says nothing either way.
    Unknown,
}

impl Probe {
    fn from_answer(answer: nix::Result<()>) -> Self {
        match answer {
            Ok(()) => Self::Alive,
            Err(nix::errno::Errno::ESRCH) => Self::Empty,
            Err(nix::errno::Errno::EPERM) => Self::Denied,
            Err(_) => Self::Unknown,
        }
    }
}

/// What the timeout path saw of the child, its pipes and its group.
#[derive(Clone, Copy, Debug)]
struct Observed {
    route: Route,
    reaped: Reaped,
    /// Both pipe readers reached EOF within the bound.
    readers_finished: bool,
    probe: Probe,
}

/// Whether a timed-out tool is gone, judged from [`Observed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Every observation a surviving tool cannot produce holds.
    Gone,
    /// Nothing shows a survivor, but nothing proves the tool gone either.
    MayRun,
    /// A process was seen after the kill: the group probe found one, or one
    /// still holds the pipes.
    Survivor,
}

impl Observed {
    /// The tool counts as gone only on a positive check. Both routes need EOF
    /// on both pipes and an empty group. On the direct route the child is the
    /// tool, so its reaped exit, after the `SIGKILL` too, means it ended. The
    /// elevation command exits only after its command does, but a `SIGKILL`
    /// ends it without the tool, so there only a reap before any `SIGKILL`
    /// counts.
    fn verdict(self) -> Verdict {
        if matches!(self.probe, Probe::Alive | Probe::Denied) || !self.readers_finished {
            return Verdict::Survivor;
        }
        let reaped = match self.route {
            Route::Direct => matches!(self.reaped, Reaped::BeforeKill | Reaped::AfterKill),
            Route::Elevated => self.reaped == Reaped::BeforeKill,
        };
        if reaped && self.probe == Probe::Empty {
            Verdict::Gone
        } else {
            Verdict::MayRun
        }
    }
}

/// Drains one of a child's pipes on its own thread.
pub(super) fn spawn_reader<R: Read + Send + 'static>(
    mut pipe: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Joins `threads`, giving them until `deadline` to finish.
///
/// A thread still running at `deadline` is left detached and `false` is
/// returned: the caller decides what its absence means. A panicked thread
/// joins to its type's default, so the joined values keep their order.
///
/// The stdin writer in `privilege::write_via_sudo_tee` can outlive the command
/// it wrote to: a pipe holder this user may not signal keeps the read end open
/// without reading, so the writer waits for the holder's life, holding its copy
/// of the content. A pipe reader a give-up abandons is
/// left the same way, holding its buffer. Accepted: the call has already failed
/// at the bound, and both die with the process.
pub(super) fn join_bounded<T: Default + Send + 'static>(
    threads: Vec<std::thread::JoinHandle<T>>,
    deadline: std::time::Instant,
) -> (Vec<T>, bool) {
    loop {
        if threads.iter().all(std::thread::JoinHandle::is_finished) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return (Vec::new(), false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let joined = threads
        .into_iter()
        .map(|thread| thread.join().unwrap_or_default())
        .collect();
    (joined, true)
}

/// Joins the pipe readers, giving them until `deadline` to finish.
///
/// A reader ends when every holder of its write end dies. A process that
/// outlived the kill keeps one open, so a plain join could hold the run; this
/// returns `false` instead.
pub(super) fn join_readers_bounded(
    readers: Vec<std::thread::JoinHandle<Vec<u8>>>,
    deadline: std::time::Instant,
) -> (Vec<Vec<u8>>, bool) {
    join_bounded(readers, deadline)
}

/// Probes `group` with signal 0 until it answers `ESRCH` or `deadline`
/// passes: a process the kill ended stays in the group until it is reaped.
fn probe_group(group: Pid, deadline: std::time::Instant) -> Probe {
    loop {
        let probe = Probe::from_answer(killpg(group, None));
        if probe == Probe::Empty || std::time::Instant::now() >= deadline {
            return probe;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Polls `child` until it is reaped or `bound` passes; `Ok(false)` when it was
/// not.
pub(super) fn reap_within(
    child: &mut std::process::Child,
    bound: std::time::Duration,
) -> std::io::Result<bool> {
    let deadline = std::time::Instant::now() + bound;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Ends the child and every process it forked, giving the group a grace on
/// `SIGTERM` before `SIGKILL`, and returns when the child was reaped.
///
/// A child still there [`REAP_BOUND`] after the `SIGKILL` is left unreaped and
/// the run goes on: until it is reaped its pid, which is also the group's id,
/// cannot be reused, so the probe that follows never meets a stranger's group;
/// once it exits it stays a zombie until this process ends and init reaps it.
fn terminate_group(child: &mut std::process::Child, group: Pid) -> Reaped {
    let _ = killpg(group, Signal::SIGTERM);
    match reap_within(child, KILL_GRACE) {
        Ok(true) => return Reaped::BeforeKill,
        Ok(false) => {}
        Err(_) => return Reaped::WaitFailed,
    }
    let _ = killpg(group, Signal::SIGKILL);
    match reap_within(child, REAP_BOUND) {
        Ok(true) => Reaped::AfterKill,
        Ok(false) => Reaped::Never,
        Err(_) => Reaped::WaitFailed,
    }
}

/// Ends what is left in `group` and answers the child's exit status when it
/// can be reaped within [`REAP_BOUND`].
///
/// A give-up leaves no member of the group running, except one this user may
/// not signal and one that left the group. The `SIGKILL` goes only when a
/// signal-0 probe answers that the group still holds a process: a child the
/// caller already reaped hands its pid — the group's id — back for reuse, and
/// a group that emptied in the meantime must not pass the signal to a stranger
/// that took the id. A group that still holds one is this child's; a group that
/// emptied and had its id taken by a new leader between the probe and the kill
/// is not detected. A member this user may not signal answers `EPERM` and
/// stays, as it does on the timeout path.
pub(super) fn end_leftover_group(
    child: &mut std::process::Child,
    group: Pid,
) -> Option<std::process::ExitStatus> {
    if Probe::from_answer(killpg(group, None)) == Probe::Alive {
        let _ = killpg(group, Signal::SIGKILL);
    }
    match reap_within(child, REAP_BOUND) {
        Ok(true) => child.wait().ok(),
        _ => None,
    }
}

/// Kills the timed-out child's group and gathers what [`Observed::verdict`]
/// judges. `wait_failed` marks a child whose wait already failed, which no
/// later reap makes a positive check.
fn observe_kill(
    child: &mut std::process::Child,
    group: Pid,
    readers: Vec<std::thread::JoinHandle<Vec<u8>>>,
    route: Route,
    wait_failed: bool,
) -> Observed {
    let reaped = terminate_group(child, group);
    let deadline = std::time::Instant::now() + KILL_GRACE;
    let (_, readers_finished) = join_readers_bounded(readers, deadline);
    Observed {
        route,
        reaped: if wait_failed {
            Reaped::WaitFailed
        } else {
            reaped
        },
        readers_finished,
        probe: probe_group(group, deadline),
    }
}

/// Runs `command` in its own process group, killing the whole group once
/// `timeout` has passed.
///
/// The child is given a null stdin whatever its caller set on the command, so
/// the tool never reads the caller's stdin. Stdout and stderr are drained while
/// the tool runs, so a child that fills a pipe cannot block the wait; a killed
/// group closes its pipes and both readers finish. The readers are joined with
/// a bound, because a descendant that survived the grace would otherwise keep
/// the run waiting. A killed run sets `process_may_still_run` unless
/// [`Observed::verdict`] finds the tool gone.
pub(super) fn run_with_timeout(
    command: &mut std::process::Command,
    route: Route,
    timeout: std::time::Duration,
    tool: &str,
) -> Result<std::process::Output, ComponentFailure> {
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| install_failure(format!("failed to run {tool}: {e}")))?;
    let group = Pid::from_raw(child.id().cast_signed());
    let mut readers: Vec<std::thread::JoinHandle<Vec<u8>>> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(spawn_reader(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(spawn_reader(stderr));
    }

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (drained, finished) =
                    join_readers_bounded(readers, std::time::Instant::now() + KILL_GRACE);
                if !finished {
                    let _ = end_leftover_group(&mut child, group);
                    let mut failure =
                        install_failure(format!("{tool} exited but kept its output pipes open"));
                    failure.process_may_still_run = true;
                    return Err(failure);
                }
                let mut drained = drained.into_iter();
                let stdout = drained.next().unwrap_or_default();
                let stderr = drained.next().unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let verdict = observe_kill(&mut child, group, readers, route, false).verdict();
                let message = if verdict == Verdict::Survivor {
                    format!(
                        "{tool} timed out after {} s and is still running",
                        timeout.as_secs()
                    )
                } else {
                    format!("{tool} timed out after {} s", timeout.as_secs())
                };
                let mut failure = install_failure(message);
                failure.process_may_still_run = verdict != Verdict::Gone;
                return Err(failure);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => {
                let verdict = observe_kill(&mut child, group, readers, route, true).verdict();
                let mut failure = install_failure(format!("failed to wait for {tool}: {e}"));
                failure.process_may_still_run = verdict != Verdict::Gone;
                return Err(failure);
            }
        }
    }
}

/// The message a failed `kpackagetool6` run reports.
///
/// Both streams carry answers the tool prints (the not-installed text is on
/// stdout), so the detail is built from stderr first and then stdout, and falls
/// back to the exit status rather than reporting nothing.
fn kpackagetool_failure(output: &std::process::Output) -> ComponentFailure {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = match (stderr.trim(), stdout.trim()) {
        ("", "") => crate::error::command_failure("kpackagetool6", &output.status),
        (stderr, "") => stderr.to_string(),
        ("", stdout) => stdout.to_string(),
        (stderr, stdout) => format!("{stderr}\n{stdout}"),
    };
    install_failure(format!("kpackagetool6 failed: {detail}"))
}

/// Installs or updates a component package using `kpackagetool6`.
///
/// `-u` alone is what a stale kpackage entry needs: kpackagetool answers a
/// package whose `KPlugin.Id` has no installed directory with exit status 2 and
/// `is not installed` on either stream. That answer on the component's own
/// directory means the tool does not recognise the installed tree, and `-i`
/// would refuse it, so the package is copied over the tree instead. Every
/// other failure is reported with the tool's own output, so a failed update is
/// never turned into a remove and reinstall. Nothing is removed with `-r`.
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
    let timeout = kpackagetool_timeout();

    let (mut command, route) = kpackagetool_cmd(kpackage_type, global);
    let output = run_with_timeout(
        command.arg("-u").arg(package_dir),
        route,
        timeout,
        "kpackagetool6",
    )?;
    if output.status.success() {
        return Ok(());
    }

    let not_installed = output.status.code() == Some(2)
        && (String::from_utf8_lossy(&output.stderr).contains("is not installed")
            || String::from_utf8_lossy(&output.stdout).contains("is not installed"));
    // An id-less package draws the same answer, but it is one kpackagetool
    // refuses to install at all, so it stays a failure.
    if !not_installed || plugin_id(package_dir).is_none() {
        return Err(kpackagetool_failure(&output));
    }

    // A package with a plugin id reaches kpackagetool only when the component's
    // directory is named after that id, so that directory is the component's
    // own and is in place: the tool does not recognise it as installed, and
    // `-i` would answer "already exists". Copying the package over the tree
    // installs it.
    log::info!(
        target: "install",
        "{}: kpackagetool6 does not recognise {}, which is in place; copying the package there",
        component.name,
        component.path.display(),
    );
    atomic_install_dir(package_dir, &component.path)
}

/// Finds a package's root in an extracted tree, patches its metadata for the
/// new version, and refuses a package `kpackagetool6` cannot address because
/// it names no plugin id.
///
/// The caller decides what a refusal means: a type without a direct fallback
/// fails the update there, before its install slot is taken; a type with one
/// keeps trying, since the fallback may install the package.
pub(super) fn prepare_package(
    extract_dir: &Path,
    component: &InstalledComponent,
    new_version: &str,
) -> Result<PathBuf, ComponentFailure> {
    let package_dir = find_package_dir(extract_dir)
        .ok_or_else(|| install_failure("no metadata.json or metadata.desktop found in package"))?;

    let metadata_json = package_dir.join("metadata.json");
    let metadata_desktop = package_dir.join("metadata.desktop");

    if is_regular_file(&metadata_json) {
        // A metadata.json kpackagetool cannot read as an object is what it
        // would install blind: refuse the package before the tool runs.
        patch_metadata(&metadata_json, component.component_type, new_version)?;
    }

    if is_regular_file(&metadata_desktop)
        && let Err(e) = patch_metadata_desktop(&metadata_desktop, new_version)
    {
        log::warn!(
            target: "patch",
            "failed to patch metadata.desktop for {}: {}",
            component.name,
            e.message
        );
    }

    // kpackagetool6 addresses a package by its plugin id, and a package with
    // none is one it never installs. A type with a direct fallback may still
    // install such a package, so only the type without one refuses here,
    // where no step has written the component.
    if !component.component_type.has_direct_fallback() && plugin_id(&package_dir).is_none() {
        return Err(install_failure(
            "the package records no plugin id, which kpackagetool6 needs to install it",
        ));
    }

    Ok(package_dir)
}

/// Installs the package root [`prepare_package`] patched.
///
/// A component whose directory is named differently from the package's plugin
/// id is installed at its own path instead: kpackagetool addresses an
/// installed package by its id, so `-u` would answer "not installed" and `-i`
/// would add a second tree under the id, leaving this one stale.
pub(super) fn install_prepared_package(
    package_dir: &Path,
    component: &InstalledComponent,
) -> Result<(), ComponentFailure> {
    if let Some(package_id) = plugin_id(package_dir)
        && package_id != component.directory_name
    {
        log::info!(
            target: "install",
            "installing {} directly: its directory name differs from the plugin id {:?}",
            component.name,
            package_id,
        );
        return atomic_install_dir(package_dir, &component.path);
    }

    let is_global = privilege::is_system_path(&component.path);
    install_via_kpackagetool(package_dir, component, is_global)
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
        if !is_regular_file(path) {
            return false;
        }
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
            is_regular_file(&dir.join("decoration.svg")) || is_directory(&dir.join("aurorae"))
        }
        ComponentType::GlobalTheme | ComponentType::SplashScreen => {
            is_regular_file(&dir.join("metadata.json"))
                || is_regular_file(&dir.join("metadata.desktop"))
        }
        ComponentType::PlasmaStyle => {
            is_directory(&dir.join("colors"))
                || is_directory(&dir.join("widgets"))
                || is_regular_file(&dir.join("metadata.desktop"))
        }
        ComponentType::SddmTheme => {
            is_regular_file(&dir.join("theme.conf")) || is_regular_file(&dir.join("Main.qml"))
        }
        ComponentType::KWinSwitcher => {
            is_regular_file(&dir.join("metadata.json")) || is_directory(&dir.join("contents"))
        }
        ComponentType::PlasmaWidget
        | ComponentType::WallpaperPlugin
        | ComponentType::KWinEffect
        | ComponentType::KWinScript
        | ComponentType::IconTheme
        | ComponentType::Wallpaper
        | ComponentType::ColorScheme
        | ComponentType::CursorTheme
        | ComponentType::ComicPlugin => false,
    }
}

fn find_index_theme_dirs(extract_dir: &Path) -> Vec<PathBuf> {
    find_all_in_dir(extract_dir, |d| is_regular_file(&d.join("index.theme")))
}

/// Locates the theme directory an icon or cursor theme's archive installs.
///
/// A multi-variant pack ships several theme dirs in one archive; the variant
/// named like the component is the one it owns. With no same-named variant, a
/// single-theme archive still installs as before, and several variants none of
/// which match is an error rather than a guess.
fn locate_index_theme_dir(
    extract_dir: &Path,
    directory_name: &str,
) -> Result<PathBuf, ComponentFailure> {
    let themes = find_index_theme_dirs(extract_dir);
    if themes.is_empty() {
        return Err(install_failure("no theme (index.theme) found in archive"));
    }
    themes
        .iter()
        .map(PathBuf::as_path)
        .find(|dir| dir.file_name().is_some_and(|name| name == directory_name))
        .or_else(|| (themes.len() == 1).then(|| themes[0].as_path()))
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            // Sorted: directory iteration order is not stable, and the message
            // names the variants in a fixed order.
            let mut variants: Vec<String> = themes
                .iter()
                .filter_map(|dir| dir.file_name()?.to_str())
                .map(|name| format!("{name:?}"))
                .collect();
            variants.sort();
            install_failure(format!(
                "archive holds several themes ({}) and none is named {directory_name:?}",
                variants.join(", ")
            ))
        })
}

fn find_wallpaper_source(extract_dir: &Path) -> Option<PathBuf> {
    // directory-based wallpaper (with contents/ or metadata.json)
    if let Some(dir) = find_in_dir(extract_dir, |d| {
        is_directory(&d.join("contents")) || is_regular_file(&d.join("metadata.json"))
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

/// The one image an unpacked wallpaper download holds, when the extract root
/// holds nothing else. An archive holding a second entry takes the package
/// path instead, so no entry it holds beside the image is dropped unseen.
pub(super) fn loose_wallpaper_image(extract_dir: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(extract_dir).ok()?;
    entries.next()?.ok()?;
    if entries.next().is_some() {
        return None;
    }
    find_wallpaper_source(extract_dir).filter(|source| is_regular_file(source))
}

/// Whether the component is a wallpaper installed as a directory, the shape
/// that decides whether a download holding one image rebuilds that directory.
pub(super) fn is_directory_wallpaper(component: &InstalledComponent) -> bool {
    component.component_type == ComponentType::Wallpaper && is_directory(&component.path)
}

/// Builds at `tree` the directory the wallpaper installed at `installed`
/// becomes when its store entry serves the one image at `image`, named
/// `image_name`. `image` is moved into `tree`; the installed directory is only
/// read.
///
/// A package directory (one holding `metadata.json`, `metadata.desktop` or
/// `contents/`) keeps its metadata files byte for byte and holds the image at
/// `contents/images/<W>x<H>.<ext>`, the name Plasma's own wallpaper packages
/// give their images; an SVG has no pixel size and keeps its own name there.
/// Any other directory ends holding only the image under its own name. Every
/// other file the directory held goes, as with any directory update.
///
/// Returns whether the tree kept the installed `metadata.json`: only a kept one
/// is the component's metadata, so only a kept one takes the new version.
pub(super) fn rebuild_wallpaper_dir(
    image: &Path,
    image_name: ImageName<'_>,
    installed: &Path,
    tree: &Path,
) -> Result<bool, ComponentFailure> {
    let metadata: Vec<&str> = ["metadata.json", "metadata.desktop"]
        .into_iter()
        .filter(|name| is_regular_file(&installed.join(name)))
        .collect();
    let target = if metadata.is_empty() && !is_directory(&installed.join("contents")) {
        tree.join(
            image_name
                .file_name()
                .ok_or_else(|| image_name.no_file_name())?,
        )
    } else {
        tree.join("contents/images")
            .join(package_image_name(image, image_name)?)
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, parent, e))?;
    }
    fs::rename(image, &target)
        .map_err(|e| ComponentFailure::io(FailureKind::Install, &target, e))?;
    let kept_json = metadata.contains(&"metadata.json");
    for name in metadata {
        let source = installed.join(name);
        let kept = tree.join(name);
        // A fresh file rather than `fs::copy`, which would carry over a
        // read-only mode the version patch cannot write through.
        let mut reader = fs::File::open(&source)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, &source, e))?;
        let mut writer = fs::File::create(&kept)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, &kept, e))?;
        std::io::copy(&mut reader, &mut writer)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, &kept, e))?;
    }
    Ok(kept_json)
}

/// The name a rebuilt wallpaper's image lands under: an archive member's own
/// name, or the name the store served the download under.
///
/// A served name whose `%xx` escapes do not decode to UTF-8 names no file, and
/// `Served` carries it as the store spelled it, which is how the refusal names
/// it; an arm that names its image by its pixel size never reads it.
#[derive(Clone, Copy)]
pub(super) enum ImageName<'a> {
    /// The store's served file name, decoded, or its spelling when its escapes
    /// do not decode to UTF-8.
    Served(Result<&'a str, &'a str>),
    /// An archive member's own name.
    Member(&'a OsStr),
}

impl<'a> ImageName<'a> {
    /// The name as a file name of its own, when it is one.
    fn file_name(&self) -> Option<&'a OsStr> {
        match self {
            Self::Served(Ok(name)) => own_file_name(OsStr::new(*name)),
            Self::Served(Err(_)) => None,
            Self::Member(name) => own_file_name(name),
        }
    }

    /// The no-file-name failure, naming the image the way the store served it.
    fn no_file_name(&self) -> ComponentFailure {
        install_failure(format!(
            "downloaded image has no file name to put in the wallpaper folder ({:?})",
            self.shown()
        ))
    }

    /// The failure of a pixel size that cannot name the image.
    fn size_failure(&self, why: &str) -> ComponentFailure {
        install_failure(format!(
            "cannot read the pixel size of downloaded image {:?} to name it in the wallpaper folder ({why})",
            self.shown()
        ))
    }

    /// The image's name for a line that prints it.
    fn shown(&self) -> String {
        match self {
            Self::Served(Ok(name)) => (*name).to_string(),
            Self::Served(Err(raw)) => (*raw).to_string(),
            Self::Member(name) => name.to_string_lossy().into_owned(),
        }
    }
}

/// The name a package directory gives the image: `<W>x<H>.<ext>` read from its
/// header, or an SVG's own name, the one image format whose name a package
/// keeps. Only that name is read, so a name that cannot be a file name is
/// refused only when no pixel size can name the image instead.
fn package_image_name(
    image: &Path,
    image_name: ImageName<'_>,
) -> Result<OsString, ComponentFailure> {
    let name = image_name.file_name();
    if let Some(name) = name
        && has_svg_extension(name)
    {
        return Ok(name.to_os_string());
    }
    match pixel_size(image) {
        Ok((size, extension)) => Ok(format!("{}x{}.{extension}", size.width, size.height).into()),
        Err(_) if name.is_none() => Err(image_name.no_file_name()),
        Err(why) => Err(image_name.size_failure(&why)),
    }
}

/// `name` as a file name of its own, when it is one: a name that is empty,
/// `.` or `..`, or that holds a path separator or a NUL, names no file.
fn own_file_name(name: &OsStr) -> Option<&OsStr> {
    Path::new(name)
        .file_name()
        .filter(|file_name| *file_name == name && !file_name.as_encoded_bytes().contains(&0))
}

/// Whether `name` ends in `.svg`, case-insensitive, the one image format a
/// wallpaper package keeps under its own name.
fn has_svg_extension(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("svg"))
}

/// An image's pixel size and the extension its format takes, read from its
/// header alone.
fn pixel_size(image: &Path) -> Result<(imagesize::ImageSize, &'static str), String> {
    let mut reader = std::io::BufReader::new(fs::File::open(image).map_err(|e| e.to_string())?);
    let kind = imagesize::reader_type(&mut reader).map_err(|e| e.to_string())?;
    let extension = match kind {
        imagesize::ImageType::Png => "png",
        imagesize::ImageType::Jpeg => "jpg",
        imagesize::ImageType::Webp => "webp",
        // Another crate in the build can turn on more of imagesize's formats.
        _ => return Err(UNSUPPORTED_IMAGE.to_string()),
    };
    let size = kind.reader_size(&mut reader).map_err(|e| e.to_string())?;
    Ok((size, extension))
}

// --- Direct Installation Methods ---

/// The file or directory a direct install copies into the component's path,
/// resolved from the unpacked tree.
#[derive(Debug)]
pub(super) enum DirectSource {
    /// A component whose installed form is one file.
    File(PathBuf),
    /// A component whose installed form is one directory.
    Dir(PathBuf),
    /// A wallpaper: one image file, or a directory of them.
    Wallpaper(PathBuf),
    /// A theme directory installed as a whole, under the name its type gives
    /// it in the log.
    ThemeDir {
        source: PathBuf,
        component_type: ComponentType,
    },
}

/// Resolves what the component's type installs by from the unpacked tree.
///
/// A lookup that finds nothing fails here, before the install slot is taken,
/// so its failure takes no backup and restores nothing. The one caller that
/// runs after a slot is taken is the fallback of a type whose `kpackagetool6`
/// install failed: the tool may have written the component by then, so a
/// package its fallback installs is never refused up front.
pub(super) fn resolve_direct_source(
    extract_dir: &Path,
    component: &InstalledComponent,
) -> Result<DirectSource, ComponentFailure> {
    match component.component_type {
        ComponentType::ColorScheme => locate_color_scheme_file(extract_dir)
            .map(DirectSource::File)
            .ok_or_else(|| install_failure("no color scheme file found in archive")),
        ComponentType::IconTheme | ComponentType::CursorTheme => {
            locate_index_theme_dir(extract_dir, &component.directory_name).map(DirectSource::Dir)
        }
        ComponentType::Wallpaper => find_wallpaper_source(extract_dir)
            .map(DirectSource::Wallpaper)
            .ok_or_else(|| install_failure("no wallpaper found in archive")),
        ComponentType::AuroraeDecoration
        | ComponentType::GlobalTheme
        | ComponentType::PlasmaStyle
        | ComponentType::SplashScreen
        | ComponentType::SddmTheme => {
            find_component_root_in_archive(extract_dir, component.component_type)
                .map(|source| DirectSource::ThemeDir {
                    source,
                    component_type: component.component_type,
                })
                .ok_or_else(|| {
                    install_failure(format!(
                        "no valid {} structure found in archive",
                        component.component_type
                    ))
                })
        }
        ComponentType::PlasmaWidget
        | ComponentType::WallpaperPlugin
        | ComponentType::KWinEffect
        | ComponentType::KWinScript
        | ComponentType::KWinSwitcher
        | ComponentType::ComicPlugin => Err(install_failure(format!(
            "{} should use kpackagetool",
            component.component_type
        ))),
    }
}

/// Installs what [`resolve_direct_source`] resolved, using direct file
/// operations (not kpackagetool).
pub(super) fn install_direct(
    source: &DirectSource,
    component: &InstalledComponent,
) -> Result<(), ComponentFailure> {
    match source {
        DirectSource::File(file) => install_color_scheme(file, &component.path),
        DirectSource::Dir(dir) => install_index_theme(dir, &component.path),
        DirectSource::Wallpaper(source) => install_wallpaper(source, &component.path),
        DirectSource::ThemeDir {
            source,
            component_type,
        } => install_theme_dir(source, &component.path, *component_type),
    }
}

fn install_color_scheme(color_file: &Path, dest_path: &Path) -> Result<(), ComponentFailure> {
    atomic_install_file(color_file, dest_path)?;
    log::debug!(target: "install", "copied color scheme to {}", dest_path.display());
    Ok(())
}

/// Installs the directory holding `index.theme` (an icon or cursor theme).
fn install_index_theme(source_dir: &Path, dest_dir: &Path) -> Result<(), ComponentFailure> {
    atomic_install_dir(source_dir, dest_dir)?;
    log::debug!(target: "install", "copied theme to {}", dest_dir.display());
    Ok(())
}

pub(super) fn install_wallpaper(source: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    if source.is_file() {
        atomic_install_file(source, dest)?;
        log::debug!(target: "install", "copied wallpaper to {}", dest.display());
    } else {
        atomic_install_dir(source, dest)?;
        log::debug!(target: "install", "copied wallpaper dir to {}", dest.display());
    }
    Ok(())
}

fn install_theme_dir(
    source_dir: &Path,
    dest_dir: &Path,
    component_type: ComponentType,
) -> Result<(), ComponentFailure> {
    atomic_install_dir(source_dir, dest_dir)?;
    log::debug!(target: "install", "copied {} to {}", component_type, dest_dir.display());
    Ok(())
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
pub(in crate::installer) mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testutil::logs::{capture_logs, captured_logs};

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

    pub(in crate::installer) fn write_script(path: &Path, body: &str) {
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
    pub(in crate::installer) fn run_child(
        test_name: &str,
        bin: &Path,
        envs: &[(&str, &str)],
    ) -> std::process::Output {
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
if [ "$1" = -n ]; then shift; fi
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
if [ "$1" = -n ]; then shift; fi
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
            find_index_theme_dirs(&extract).is_empty(),
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
    fn plugin_id_reads_from_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = dir.path().join("metadata.json");
        std::fs::write(
            &metadata,
            r#"{"KPlugin": {"Id": "org.kde.actual.id", "Name": "Test"}}"#,
        )
        .unwrap();

        assert_eq!(plugin_id(dir.path()).as_deref(), Some("org.kde.actual.id"));
    }

    #[test]
    fn plugin_id_is_absent_without_metadata() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(plugin_id(dir.path()), None);
    }

    #[test]
    fn plugin_id_reads_from_metadata_desktop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("metadata.desktop"),
            "[Desktop Entry]\nName=Test\nX-KDE-PluginInfo-Name=org.kde.desktop.id\n",
        )
        .unwrap();

        assert_eq!(plugin_id(dir.path()).as_deref(), Some("org.kde.desktop.id"));
    }

    /// An installed `metadata.json` past the cap is skipped with a warning, so
    /// the installed side names no id and the check has nothing to compare.
    #[test]
    fn an_oversized_installed_metadata_json_leaves_the_plugin_id_check_unanswered() {
        capture_logs();
        let installed = tempfile::tempdir().unwrap();
        std::fs::write(
            installed.path().join("metadata.json"),
            format!(
                r#"{{"KPlugin": {{"Id": "org.installed.widget"}}, "Pad": "{}"}}"#,
                "x".repeat(MAX_METADATA_BYTES as usize)
            ),
        )
        .unwrap();
        let component = widget_at(installed.path(), "org.installed.widget");
        let extract = tempfile::tempdir().unwrap();
        let package = extract.path().join("org.other.widget");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(
            package.join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.other.widget"}}"#,
        )
        .unwrap();

        verify_package_plugin_id(extract.path(), &component).unwrap();

        assert!(
            captured_logs()
                .iter()
                .any(|line| line.contains("metadata cap")),
            "the skipped installed metadata was not reported: {:?}",
            captured_logs()
        );
    }

    #[test]
    fn verify_package_plugin_id_refuses_a_desktop_only_package_of_another_widget() {
        let installed = tempfile::tempdir().unwrap();
        std::fs::write(
            installed.path().join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.installed.widget"}}"#,
        )
        .unwrap();
        let component = InstalledComponent {
            name: "Installed Widget".to_string(),
            directory_name: "org.installed.widget".to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path: installed.path().to_path_buf(),
            is_system: false,
            release_date: String::new(),
        };
        let extract = tempfile::tempdir().unwrap();
        let package = extract.path().join("org.other.widget");
        std::fs::create_dir(&package).unwrap();
        std::fs::write(
            package.join("metadata.desktop"),
            "[Desktop Entry]\nX-KDE-PluginInfo-Name=org.other.widget\n",
        )
        .unwrap();

        let refusal = verify_package_plugin_id(extract.path(), &component).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal
                .message
                .contains("store entry is a different widget"),
            "{}",
            refusal.message
        );
    }

    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "mkfifo {} failed", path.display());
    }

    /// Runs `body` on its own thread and fails the test when it has not
    /// returned within five seconds: the defect these tests pin is a read that
    /// blocks forever, which must red the test rather than hang the suite.
    fn within_five_seconds<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(body());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the call blocked for five seconds")
    }

    #[test]
    fn patch_metadata_rejects_a_metadata_json_that_is_not_an_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();

        let result = patch_metadata(&path, ComponentType::PlasmaWidget, "2.0");

        assert!(result.is_err(), "a JSON array was patched: {result:?}");
    }

    #[test]
    fn patch_metadata_survives_a_kplugin_that_is_not_an_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(&path, r#"{"KPlugin": "not an object"}"#).unwrap();

        // Either outcome is fine; a panic, which would unwind through the
        // update pool and kill the whole run, is not.
        let _ = patch_metadata(&path, ComponentType::PlasmaWidget, "2.0");
    }

    #[test]
    fn a_symlinked_package_metadata_json_is_refused() {
        let package = tempfile::tempdir().unwrap();
        std::fs::write(package.path().join("metadata.desktop"), "[Desktop Entry]\n").unwrap();
        let real = package.path().join("real.json");
        std::fs::write(&real, r#"{"KPlugin": {"Id": "org.other.widget"}}"#).unwrap();
        std::os::unix::fs::symlink(&real, package.path().join("metadata.json")).unwrap();

        let refusal = verify_package_metadata(package.path()).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal.message.contains("metadata.json"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_fifo_package_metadata_json_is_refused() {
        let package = tempfile::tempdir().unwrap();
        std::fs::write(package.path().join("metadata.desktop"), "[Desktop Entry]\n").unwrap();
        mkfifo(&package.path().join("metadata.json"));

        let refusal = verify_package_metadata(package.path()).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal.message.contains("metadata.json"),
            "{}",
            refusal.message
        );
    }

    /// A component whose installed tree is `path`, for the plugin-id checks.
    fn widget_at(path: &Path, directory_name: &str) -> InstalledComponent {
        InstalledComponent {
            name: "Installed Widget".to_string(),
            directory_name: directory_name.to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path: path.to_path_buf(),
            is_system: false,
            release_date: String::new(),
        }
    }

    /// An installed widget whose `metadata.json` names `id`, and an extract
    /// directory holding a package at `package_name`.
    fn plugin_id_fixture(
        installed_id: &str,
        package_name: &str,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        InstalledComponent,
        PathBuf,
    ) {
        let installed = tempfile::tempdir().unwrap();
        std::fs::write(
            installed.path().join("metadata.json"),
            format!(r#"{{"KPlugin": {{"Id": "{installed_id}"}}}}"#),
        )
        .unwrap();
        let component = widget_at(installed.path(), installed_id);
        let extract = tempfile::tempdir().unwrap();
        let package = extract.path().join(package_name);
        std::fs::create_dir(&package).unwrap();
        (installed, extract, component, package)
    }

    fn assert_different_widget(refusal: &ComponentFailure) {
        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal
                .message
                .contains("store entry is a different widget"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_symlinked_package_metadata_desktop_is_refused() {
        let package = tempfile::tempdir().unwrap();
        std::fs::write(package.path().join("metadata.json"), "{}").unwrap();
        let real = package.path().join("real.desktop");
        std::fs::write(&real, "[Desktop Entry]\n").unwrap();
        std::os::unix::fs::symlink(&real, package.path().join("metadata.desktop")).unwrap();

        let refusal = verify_package_metadata(package.path()).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal.message.contains("metadata.desktop"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_fifo_package_metadata_desktop_is_refused() {
        let package = tempfile::tempdir().unwrap();
        std::fs::write(package.path().join("metadata.json"), "{}").unwrap();
        mkfifo(&package.path().join("metadata.desktop"));

        let refusal = verify_package_metadata(package.path()).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal.message.contains("metadata.desktop"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_package_desktop_id_written_with_spaced_equals_is_read() {
        let (_installed, extract, component, package) =
            plugin_id_fixture("org.installed.widget", "org.installed.widget");
        std::fs::write(
            package.join("metadata.desktop"),
            "[Desktop Entry]\nX-KDE-PluginInfo-Name = org.other.widget\n",
        )
        .unwrap();

        let refusal = verify_package_plugin_id(extract.path(), &component).unwrap_err();

        assert_different_widget(&refusal);
    }

    #[test]
    fn a_duplicate_package_desktop_id_takes_the_last_value() {
        let (_installed, extract, component, package) =
            plugin_id_fixture("org.installed.widget", "org.installed.widget");
        std::fs::write(
            package.join("metadata.desktop"),
            "[Desktop Entry]\n\
             X-KDE-PluginInfo-Name=org.installed.widget\n\
             X-KDE-PluginInfo-Name=org.other.widget\n",
        )
        .unwrap();

        let refusal = verify_package_plugin_id(extract.path(), &component).unwrap_err();

        assert_different_widget(&refusal);
    }

    #[test]
    fn a_package_desktop_id_in_another_group_is_not_the_plugin_id() {
        let (_installed, extract, component, package) =
            plugin_id_fixture("org.installed.widget", "org.installed.widget");
        std::fs::write(
            package.join("metadata.desktop"),
            "[Other Group]\n\
             X-KDE-PluginInfo-Name=org.installed.widget\n\
             [Desktop Entry]\n\
             X-KDE-PluginInfo-Name=org.other.widget\n",
        )
        .unwrap();

        let refusal = verify_package_plugin_id(extract.path(), &component).unwrap_err();

        assert_different_widget(&refusal);
    }

    #[test]
    fn a_package_json_id_after_a_utf8_bom_is_read() {
        let (_installed, extract, component, package) =
            plugin_id_fixture("org.installed.widget", "org.installed.widget");
        std::fs::write(
            package.join("metadata.json"),
            "\u{feff}{\"KPlugin\": {\"Id\": \"org.other.widget\"}}",
        )
        .unwrap();

        let refusal = verify_package_plugin_id(extract.path(), &component).unwrap_err();

        assert_different_widget(&refusal);
    }

    #[test]
    fn a_package_with_no_plugin_id_is_refused_when_the_installed_side_has_one() {
        let (_installed, extract, component, package) =
            plugin_id_fixture("org.installed.widget", "org.installed.widget");
        std::fs::write(
            package.join("metadata.desktop"),
            "[Desktop Entry]\nName=Nameless\n",
        )
        .unwrap();

        let refusal = verify_package_plugin_id(extract.path(), &component).unwrap_err();

        assert_different_widget(&refusal);
    }

    #[test]
    fn a_package_link_leaving_its_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        std::fs::create_dir_all(package.join("sub")).unwrap();
        std::os::unix::fs::symlink("../../outside", package.join("sub").join("link")).unwrap();

        let refusal = verify_package_tree(&package, ComponentType::PlasmaWidget).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal.message.contains("outside the package root"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn an_absolute_package_link_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        std::fs::create_dir(&package).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", package.join("link")).unwrap();

        let refusal = verify_package_tree(&package, ComponentType::IconTheme).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal.message.contains("absolute symlink"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_chain_of_package_links_leaving_the_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        std::fs::create_dir_all(package.join("a/b")).unwrap();
        std::os::unix::fs::symlink("../..", package.join("a/b/s")).unwrap();
        std::os::unix::fs::symlink("s/../../..", package.join("a/b/l")).unwrap();

        let refusal = verify_package_tree(&package, ComponentType::PlasmaWidget).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(
            refusal.message.contains("outside the package root"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_package_directory_link_is_refused_for_kpackagetool() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        std::fs::create_dir_all(package.join("contents/ui")).unwrap();
        std::fs::write(package.join("contents/ui/main.qml"), "Item {}").unwrap();
        std::os::unix::fs::symlink("..", package.join("contents/ui/loop")).unwrap();

        let refusal = verify_package_tree(&package, ComponentType::PlasmaWidget).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(refusal.message.contains("directory"), "{}", refusal.message);
    }

    #[test]
    fn a_package_link_staying_inside_its_root_is_allowed() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        std::fs::create_dir_all(package.join("sub")).unwrap();
        std::fs::write(package.join("target.txt"), b"inside").unwrap();
        std::os::unix::fs::symlink("../target.txt", package.join("sub").join("link")).unwrap();

        verify_package_tree(&package, ComponentType::IconTheme).unwrap();
    }

    #[test]
    fn a_directory_link_staying_inside_an_icon_theme_is_allowed() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        std::fs::create_dir_all(package.join("scalable/apps")).unwrap();
        std::fs::write(package.join("scalable/apps/app.svg"), "<svg/>").unwrap();
        std::os::unix::fs::symlink("..", package.join("scalable/apps/loop")).unwrap();

        verify_package_tree(&package, ComponentType::IconTheme).unwrap();
    }

    fn theme_component(
        path: &Path,
        directory_name: &str,
        component_type: ComponentType,
    ) -> InstalledComponent {
        InstalledComponent {
            name: directory_name.to_string(),
            directory_name: directory_name.to_string(),
            version: "1.0".to_string(),
            component_type,
            path: path.to_path_buf(),
            is_system: false,
            release_date: String::new(),
        }
    }

    /// A two-variant cursor pack: `left` and `right` each carry an `index.theme`
    /// and one cursor file whose content names the variant.
    fn two_variant_extract(root: &Path, left: &str, right: &str) -> PathBuf {
        let extract = root.join("extract");
        for name in [left, right] {
            let variant = extract.join(name);
            std::fs::create_dir_all(variant.join("cursors")).unwrap();
            std::fs::write(variant.join("index.theme"), "[Icon Theme]\n").unwrap();
            std::fs::write(variant.join("cursors/left_ptr"), name).unwrap();
        }
        extract
    }

    #[test]
    fn a_variant_pack_installs_the_variant_named_like_the_component() {
        for names in [
            ("Aaa-cursors", "Zzz-cursors"),
            ("Zzz-cursors", "Aaa-cursors"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let extract = two_variant_extract(dir.path(), names.0, names.1);
            let dest = dir.path().join("Aaa-cursors");
            let component = theme_component(&dest, "Aaa-cursors", ComponentType::CursorTheme);

            let source = resolve_direct_source(&extract, &component).unwrap();
            install_direct(&source, &component).unwrap();

            assert_eq!(
                std::fs::read_to_string(dest.join("cursors/left_ptr")).unwrap(),
                "Aaa-cursors",
                "installed a sibling variant (extract order {names:?})"
            );
        }
    }

    #[test]
    fn a_variant_pack_with_no_matching_variant_name_fails_the_install() {
        let dir = tempfile::tempdir().unwrap();
        let extract = two_variant_extract(dir.path(), "Aaa-cursors", "Zzz-cursors");
        let dest = dir.path().join("Other-cursors");
        let component = theme_component(&dest, "Other-cursors", ComponentType::CursorTheme);

        let failure = resolve_direct_source(&extract, &component).unwrap_err();

        assert_eq!(failure.kind, FailureKind::Install);
        assert!(
            failure.message.contains("Aaa-cursors") && failure.message.contains("Zzz-cursors"),
            "the failure did not name the variants: {}",
            failure.message
        );
        assert_eq!(
            failure.message,
            "archive holds several themes (\"Aaa-cursors\", \"Zzz-cursors\") and none is named \"Other-cursors\""
        );
    }

    #[test]
    fn an_archive_without_a_theme_fails_with_the_no_theme_message() {
        let dir = tempfile::tempdir().unwrap();
        let extract = dir.path().join("extract");
        let variant = extract.join("Lonely-cursors");
        std::fs::create_dir_all(&variant).unwrap();
        std::fs::write(variant.join("README"), "no theme here").unwrap();
        let dest = dir.path().join("Lonely-cursors");
        let component = theme_component(&dest, "Lonely-cursors", ComponentType::CursorTheme);

        let failure = resolve_direct_source(&extract, &component).unwrap_err();

        assert_eq!(failure.kind, FailureKind::Install);
        assert_eq!(failure.message, "no theme (index.theme) found in archive");
    }

    #[test]
    fn a_single_theme_archive_with_another_name_still_installs() {
        let dir = tempfile::tempdir().unwrap();
        let extract = dir.path().join("extract");
        let variant = extract.join("Other-cursors");
        std::fs::create_dir_all(variant.join("cursors")).unwrap();
        std::fs::write(variant.join("index.theme"), "[Icon Theme]\n").unwrap();
        std::fs::write(variant.join("cursors/left_ptr"), "single").unwrap();
        let dest = dir.path().join("Aaa-cursors");
        let component = theme_component(&dest, "Aaa-cursors", ComponentType::CursorTheme);

        let source = resolve_direct_source(&extract, &component).unwrap();
        install_direct(&source, &component).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("cursors/left_ptr")).unwrap(),
            "single",
            "the single-theme fallback no longer installs"
        );
    }

    #[test]
    fn patch_metadata_refuses_a_metadata_json_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"KPlugin": {{}}, "Pad": "{}"}}"#,
                "x".repeat(2 * 1024 * 1024)
            ),
        )
        .unwrap();

        let refusal = patch_metadata(&path, ComponentType::PlasmaWidget, "2.0").unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert!(refusal.message.contains("cap"), "{}", refusal.message);
    }

    #[test]
    fn a_present_component_beside_a_stale_old_sibling_is_not_stranded() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("org.test.widget");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("metadata.json"), b"{}").unwrap();
        let stale = temp_sibling(&dest, TEMP_OLD_SUFFIX);
        std::fs::create_dir_all(&stale).unwrap();

        assert!(
            stranded_in_dir(dir.path()).is_empty(),
            "a present component's dead old copy was reported stranded"
        );

        // Control: with the component gone, the same sibling is stranded.
        std::fs::remove_dir_all(&dest).unwrap();
        assert_eq!(stranded_in_dir(dir.path()).len(), 1);
        assert_eq!(
            stranded_in_dir(dir.path())[0].1,
            dir.path().join("org.test.widget"),
            "a stranded component is not named by the path it is restored to"
        );
    }

    #[test]
    fn the_process_group_probe_sees_a_live_group_and_a_dead_one() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_raw(child.id() as i32);

        assert_eq!(
            probe_group(group, std::time::Instant::now()),
            Probe::Alive,
            "a live group was read as gone"
        );

        let _ = killpg(group, Signal::SIGKILL);
        let _ = child.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

        assert_eq!(
            probe_group(group, deadline),
            Probe::Empty,
            "a dead group was read as alive"
        );
    }

    const HUNG_CHILD: &str =
        "installer::install::tests::a_hung_kpackagetool_is_killed_at_the_timeout";

    /// A fake named `kpackagetool6` that forks a helper which keeps the pipes
    /// open, then sleeps: killing only the direct child leaves the run blocked.
    const FORKING_KPACKAGETOOL: &str = "#!/bin/sh\nsh -c 'sleep 30' &\nsleep 30\n";

    #[test]
    fn a_hung_kpackagetool_is_killed_at_the_timeout() {
        if std::env::var_os("PU_TEST_HUNG_KPACKAGETOOL").is_some() {
            let extract = PathBuf::from(std::env::var("PU_TEST_EXTRACT").unwrap());
            let component = widget_at(
                Path::new(&std::env::var("PU_TEST_COMPONENT").unwrap()),
                "org.test.widget",
            );
            let started = std::time::Instant::now();

            let package_dir = prepare_package(&extract, &component, "2.0").unwrap();
            let refusal = install_prepared_package(&package_dir, &component).unwrap_err();

            assert_eq!(refusal.kind, FailureKind::Install);
            assert!(refusal.message.contains("timed out"), "{}", refusal.message);
            assert!(
                started.elapsed() < std::time::Duration::from_secs(3),
                "a forked kpackagetool kept the run: {:?}",
                started.elapsed()
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("kpackagetool6"), FORKING_KPACKAGETOOL);
        let extract = dir.path().join("extract");
        std::fs::create_dir_all(extract.join("org.test.widget")).unwrap();
        std::fs::write(
            extract.join("org.test.widget").join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget"}}"#,
        )
        .unwrap();
        let component = dir.path().join("component");
        let done = dir.path().join("child-done");

        let output = run_child(
            HUNG_CHILD,
            &bin,
            &[
                ("PU_TEST_HUNG_KPACKAGETOOL", "1"),
                ("PU_TEST_KPACKAGETOOL_TIMEOUT_MS", "100"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_EXTRACT", extract.to_str().unwrap()),
                ("PU_TEST_COMPONENT", component.to_str().unwrap()),
            ],
        );

        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// [`run_with_timeout`] gives its child a null stdin whatever its caller
    /// set on the command, so a tool never waits on the caller's stdin.
    #[test]
    fn run_with_timeout_gives_its_child_a_null_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fd0 = dir.path().join("fd-0");
        write_script(
            &bin.join("kpackagetool6"),
            "#!/bin/sh\nreadlink /proc/self/fd/0 > \"$PU_TEST_FD0\"\nexit 0\n",
        );
        let caller_stdin = dir.path().join("caller-stdin");
        std::fs::write(&caller_stdin, b"the caller's stdin").unwrap();

        let mut command = std::process::Command::new(bin.join("kpackagetool6"));
        command
            .env("PU_TEST_FD0", &fd0)
            .stdin(std::fs::File::open(&caller_stdin).unwrap());
        let output = run_with_timeout(
            &mut command,
            Route::Direct,
            std::time::Duration::from_secs(5),
            "kpackagetool6",
        )
        .unwrap();

        assert!(output.status.success(), "{:?}", output.status);
        assert_eq!(
            std::fs::read_to_string(&fd0).unwrap().trim(),
            "/dev/null",
            "the child read the caller's stdin"
        );
    }

    const DETACHED_CHILD: &str =
        "installer::install::tests::a_timed_out_tool_sudo_detached_keeps_the_backup";

    /// A fake `sudo` that runs its command in a new session, as sudo's
    /// `use_pty` does, and outlives `SIGTERM` without passing it on: the
    /// command stays out of the process group the timeout signals.
    const DETACHING_SUDO: &str =
        "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\ntrap '' TERM\nsetsid \"$@\" &\nwait\n";

    /// A fake `kpackagetool6` that records its pid and outlives the timeout.
    const RECORDED_SLEEPING_KPACKAGETOOL: &str =
        "#!/bin/sh\necho $$ > \"$PU_TEST_TOOL_PID\"\nexec sleep 30\n";

    /// Whether `pid` names a process that has not exited; a zombie has.
    pub(in crate::installer) fn process_running(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| !rest.starts_with('Z'))
        })
    }

    #[test]
    fn a_timed_out_tool_sudo_detached_keeps_the_backup() {
        if std::env::var_os("PU_TEST_DETACHED_TOOL").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let package_dir = root.join("package");
            let backup = root.join("backup");
            let component = widget_at(&root.join("component"), "org.test.widget");
            let slot = super::super::InstallSlot::new();

            let failure = install_via_kpackagetool(&package_dir, &component, true).unwrap_err();
            assert!(
                failure.process_may_still_run,
                "a tool the timeout never observed exit was read as gone: {}",
                failure.message
            );
            let refusal = super::super::handle_installation_failure(
                &backup,
                &component,
                &failure,
                &slot.acquire(),
            )
            .unwrap_err();

            assert_eq!(refusal.kind, FailureKind::Install);
            assert!(
                refusal.message.contains(&backup.display().to_string()),
                "the failure does not name the kept backup: {}",
                refusal.message
            );
            assert_eq!(
                std::fs::read_to_string(component.path.join("marker")).unwrap(),
                "being written",
                "the backup was restored over a tree the tool may still be writing"
            );
            assert_eq!(
                std::fs::read_to_string(backup.join("marker")).unwrap(),
                "old",
                "the kept backup changed"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("sudo"), DETACHING_SUDO);
        write_script(&bin.join("kpackagetool6"), RECORDED_SLEEPING_KPACKAGETOOL);
        let root = dir.path().join("root");
        let package = root.join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("component")).unwrap();
        std::fs::write(root.join("component").join("marker"), b"being written").unwrap();
        std::fs::create_dir_all(root.join("backup")).unwrap();
        std::fs::write(root.join("backup").join("marker"), b"old").unwrap();
        let tool_pid = dir.path().join("tool-pid");
        let done = dir.path().join("child-done");

        let output = run_child(
            DETACHED_CHILD,
            &bin,
            &[
                ("PU_TEST_DETACHED_TOOL", "1"),
                ("PU_TEST_KPACKAGETOOL_TIMEOUT_MS", "100"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_TOOL_PID", tool_pid.to_str().unwrap()),
            ],
        );

        let pid: i32 = std::fs::read_to_string(&tool_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let survived = process_running(pid);
        let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process_running(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !process_running(pid),
            "the fake tool {pid} outlived the test"
        );
        assert!(
            survived,
            "the fake tool had exited before the run returned, so the fixture tests nothing"
        );
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Kills the process whose pid `pid_file` holds, waits for it to go, and
    /// returns whether it was still running.
    fn stop_recorded(pid_file: &Path) -> bool {
        let pid: i32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let running = process_running(pid);
        let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process_running(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!process_running(pid), "the stub {pid} outlived the test");
        running
    }

    fn assert_child_done(output: &std::process::Output, done: &Path) {
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// A tool that exits while a process of its group still holds the output
    /// pipes leaves no process behind: the give-up ends the group before it
    /// reports the kept pipes.
    #[test]
    fn a_tool_that_exits_with_a_pipe_holder_ends_its_group() {
        if std::env::var_os("PU_TEST_KEPT_PIPES_CHILD").is_some() {
            let mut command = std::process::Command::new("kept-pipes-tool");
            let started = std::time::Instant::now();
            let failure = run_with_timeout(
                &mut command,
                Route::Direct,
                std::time::Duration::from_secs(30),
                "kept-pipes-tool",
            )
            .expect_err("the tool exits while its holder keeps the pipes");
            let elapsed = started.elapsed();
            std::fs::write(
                std::env::var_os("PU_TEST_KEPT_PIPES_ELAPSED").unwrap(),
                elapsed.as_secs_f64().to_string(),
            )
            .unwrap();
            std::fs::write(
                std::env::var_os("PU_TEST_KEPT_PIPES_RESULT").unwrap(),
                format!("{}|{}", failure.message, failure.process_may_still_run),
            )
            .unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // Exits at once while a background process inherits both output pipes
        // and outlives it.
        write_script(
            &bin.join("kept-pipes-tool"),
            "#!/bin/sh\nsleep 30 &\necho $! > \"$PU_TEST_KEPT_PIPES_PID\"\nexit 0\n",
        );
        let pid_path = dir.path().join("holder.pid");
        let elapsed_path = dir.path().join("elapsed");
        let result = dir.path().join("result");
        let path = OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));

        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "installer::install::tests::a_tool_that_exits_with_a_pipe_holder_ends_its_group",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_KEPT_PIPES_CHILD", "1")
            .env("PU_TEST_KEPT_PIPES_PID", &pid_path)
            .env("PU_TEST_KEPT_PIPES_ELAPSED", &elapsed_path)
            .env("PU_TEST_KEPT_PIPES_RESULT", &result)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let finished = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        if !finished {
            let _ = killpg(Pid::from_raw(child.id().cast_signed()), Signal::SIGKILL);
        }
        let status = child.wait().unwrap();
        let mut child_stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            drop(pipe.read_to_string(&mut child_stderr));
        }
        let holder: i32 = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The give-up must leave nothing behind. Measure it, then clean up
        // whatever it left, so a red assertion below leaves no process behind.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while process_running(holder) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let survived = process_running(holder);
        let _ = nix::sys::signal::kill(Pid::from_raw(holder), Signal::SIGKILL);
        assert!(
            finished,
            "the run did not finish within 20 s\n--- child stderr\n{child_stderr}"
        );
        assert!(
            status.success(),
            "child run failed ({status})\n--- child stderr\n{child_stderr}"
        );
        // The holder leg runs before the elapsed and line legs so it binds the
        // give-up's group kill on its own.
        assert!(!survived, "the pipe holder {holder} survived the give-up");
        let elapsed: f64 = std::fs::read_to_string(&elapsed_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let bound = KILL_GRACE + std::time::Duration::from_secs(2);
        assert!(
            elapsed < bound.as_secs_f64(),
            "the give-up waited for the pipe holder: {elapsed:.3} s"
        );
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "kept-pipes-tool exited but kept its output pipes open|true",
            "the give-up did not report the kept pipes as possibly running"
        );
    }

    #[test]
    fn the_verdict_counts_a_tool_gone_only_on_a_positive_check() {
        use Probe::{Alive, Denied, Empty, Unknown};
        use Reaped::{AfterKill, BeforeKill, Never, WaitFailed};
        use Route::{Direct, Elevated};
        use Verdict::{Gone, MayRun, Survivor};

        let rows = [
            // Direct: exits within the grace, or is killed by the SIGKILL.
            (Direct, BeforeKill, true, Empty, Gone),
            (Direct, AfterKill, true, Empty, Gone),
            // Direct: a same-group descendant outlives it.
            (Direct, BeforeKill, true, Alive, Survivor),
            // A descendant holds the pipes past the bound.
            (Direct, BeforeKill, false, Empty, Survivor),
            (Elevated, BeforeKill, false, Empty, Survivor),
            // A process this user may not signal is left.
            (Direct, AfterKill, true, Denied, Survivor),
            (Elevated, BeforeKill, true, Denied, Survivor),
            (Elevated, AfterKill, true, Alive, Survivor),
            // Elevated: sudo ends only after its command, unless SIGKILLed.
            (Elevated, BeforeKill, true, Empty, Gone),
            (Elevated, AfterKill, true, Empty, MayRun),
            // Not reaped within the bound after the SIGKILL.
            (Direct, Never, true, Empty, MayRun),
            (Elevated, Never, true, Empty, MayRun),
            // A wait error.
            (Direct, WaitFailed, true, Empty, MayRun),
            (Elevated, WaitFailed, true, Empty, MayRun),
            // A probe answer that says nothing.
            (Direct, BeforeKill, true, Unknown, MayRun),
            (Elevated, BeforeKill, true, Unknown, MayRun),
        ];
        for (route, reaped, readers_finished, probe, expected) in rows {
            let observed = Observed {
                route,
                reaped,
                readers_finished,
                probe,
            };
            assert_eq!(observed.verdict(), expected, "{observed:?}");
        }
    }

    #[test]
    fn the_group_probe_reads_eperm_as_a_live_process() {
        use nix::errno::Errno;

        assert_eq!(Probe::from_answer(Ok(())), Probe::Alive);
        assert_eq!(Probe::from_answer(Err(Errno::ESRCH)), Probe::Empty);
        assert_eq!(Probe::from_answer(Err(Errno::EPERM)), Probe::Denied);
        assert_eq!(Probe::from_answer(Err(Errno::EINVAL)), Probe::Unknown);
    }

    const WAITING_SUDO_CHILD: &str =
        "installer::install::tests::a_sudo_reaped_only_after_the_sigkill_keeps_the_backup";

    /// A fake `sudo` that outlives `SIGTERM` and becomes its command, so only
    /// the `SIGKILL` ends it, and the group is empty once it is reaped.
    const KILLED_SUDO: &str =
        "#!/bin/sh\nif [ \"$1\" = -n ]; then shift; fi\ntrap '' TERM\nexec \"$@\"\n";

    /// A `sudo` that needed the `SIGKILL` proves nothing about its command, even
    /// when the group probe then finds the group empty and both pipes closed.
    #[test]
    fn a_sudo_reaped_only_after_the_sigkill_keeps_the_backup() {
        if std::env::var_os("PU_TEST_WAITING_SUDO").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let backup = root.join("backup");
            let component = widget_at(&root.join("component"), "org.test.widget");
            let slot = super::super::InstallSlot::new();

            let failure =
                install_via_kpackagetool(&root.join("package"), &component, true).unwrap_err();
            assert_eq!(failure.message, "kpackagetool6 timed out after 0 s");
            assert!(
                failure.process_may_still_run,
                "a sudo that needed the SIGKILL was read as proof its tool is gone"
            );
            let refusal = super::super::handle_installation_failure(
                &backup,
                &component,
                &failure,
                &slot.acquire(),
            )
            .unwrap_err();
            assert_eq!(
                refusal.message,
                format!(
                    "install failed (kpackagetool6 timed out after 0 s), and the tool that failed it may still be running; the backup was kept at {}",
                    backup.display()
                )
            );
            assert_eq!(
                std::fs::read_to_string(component.path.join("marker")).unwrap(),
                "being written"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("sudo"), KILLED_SUDO);
        write_script(&bin.join("kpackagetool6"), RECORDED_SLEEPING_KPACKAGETOOL);
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("package")).unwrap();
        std::fs::write(
            root.join("package").join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("component")).unwrap();
        std::fs::write(root.join("component").join("marker"), b"being written").unwrap();
        std::fs::create_dir_all(root.join("backup")).unwrap();
        std::fs::write(root.join("backup").join("marker"), b"old").unwrap();
        let tool_pid = dir.path().join("tool-pid");
        let done = dir.path().join("child-done");

        let output = run_child(
            WAITING_SUDO_CHILD,
            &bin,
            &[
                ("PU_TEST_WAITING_SUDO", "1"),
                ("PU_TEST_KPACKAGETOOL_TIMEOUT_MS", "500"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_TOOL_PID", tool_pid.to_str().unwrap()),
            ],
        );

        let survived = stop_recorded(&tool_pid);
        assert_child_done(&output, &done);
        assert!(
            !survived,
            "the fake tool outlived the SIGKILL, so the group probe was not empty"
        );
    }

    const TERM_EXIT_CHILD: &str =
        "installer::install::tests::a_timed_out_tool_that_exits_on_sigterm_is_restored";

    /// A tool that ends on `SIGTERM` within the grace, with its group and its
    /// pipes, is gone, and its backup goes back as before.
    #[test]
    fn a_timed_out_tool_that_exits_on_sigterm_is_restored() {
        if std::env::var_os("PU_TEST_TERM_EXIT").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let backup = root.join("backup");
            let component = widget_at(&root.join("component"), "org.test.widget");
            let slot = super::super::InstallSlot::new();

            let failure =
                install_via_kpackagetool(&root.join("package"), &component, false).unwrap_err();
            assert_eq!(failure.message, "kpackagetool6 timed out after 0 s");
            assert!(
                !failure.process_may_still_run,
                "a tool that exited on SIGTERM was read as possibly running"
            );
            super::super::handle_installation_failure(
                &backup,
                &component,
                &failure,
                &slot.acquire(),
            )
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(component.path.join("marker")).unwrap(),
                "old"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("kpackagetool6"), RECORDED_SLEEPING_KPACKAGETOOL);
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("package")).unwrap();
        std::fs::write(
            root.join("package").join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("component")).unwrap();
        std::fs::write(root.join("component").join("marker"), b"being written").unwrap();
        std::fs::create_dir_all(root.join("backup")).unwrap();
        std::fs::write(root.join("backup").join("marker"), b"old").unwrap();
        let tool_pid = dir.path().join("tool-pid");
        let done = dir.path().join("child-done");

        let output = run_child(
            TERM_EXIT_CHILD,
            &bin,
            &[
                ("PU_TEST_TERM_EXIT", "1"),
                ("PU_TEST_KPACKAGETOOL_TIMEOUT_MS", "500"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_TOOL_PID", tool_pid.to_str().unwrap()),
            ],
        );

        let survived = stop_recorded(&tool_pid);
        assert_child_done(&output, &done);
        assert!(!survived, "the fake tool outlived the SIGTERM");
    }

    const UNREAPED_CHILD: &str =
        "installer::install::tests::a_child_the_sigkill_misses_does_not_hold_the_run";

    /// A fake `kpackagetool6` the group's signals never reach: it moves into
    /// its parent's process group, as a `sudo` the user may not signal stays
    /// out of the kill's reach, and closes the pipes so only the wait for it
    /// could hold the run.
    const GROUP_LEAVING_KPACKAGETOOL: &str = r#"#!/bin/sh
exec perl -e 'setpgrp(0, getpgrp(getppid())) or die; open(my $f, ">", $ENV{PU_TEST_TOOL_PID}) or die; print $f "$$\n"; close($f); sleep 30' >/dev/null 2>&1
"#;

    #[test]
    fn a_child_the_sigkill_misses_does_not_hold_the_run() {
        if std::env::var_os("PU_TEST_UNREAPED").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let component = widget_at(&root.join("component"), "org.test.widget");
            let started = std::time::Instant::now();

            let failure =
                install_via_kpackagetool(&root.join("package"), &component, false).unwrap_err();
            let elapsed = started.elapsed();

            // The 1 s timeout, the grace, the reap bound, and slack for a
            // loaded machine; the stub sleeps 30 s.
            let bound = std::time::Duration::from_secs(1 + 2) + KILL_GRACE + REAP_BOUND;
            assert!(
                elapsed < bound,
                "the wait after the SIGKILL held the run for {elapsed:?}"
            );
            assert_eq!(failure.message, "kpackagetool6 timed out after 1 s");
            assert!(failure.process_may_still_run);
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("kpackagetool6"), GROUP_LEAVING_KPACKAGETOOL);
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("package")).unwrap();
        std::fs::write(
            root.join("package").join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("component")).unwrap();
        let tool_pid = dir.path().join("tool-pid");
        let done = dir.path().join("child-done");

        let output = run_child(
            UNREAPED_CHILD,
            &bin,
            &[
                ("PU_TEST_UNREAPED", "1"),
                ("PU_TEST_KPACKAGETOOL_TIMEOUT_MS", "1000"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_TOOL_PID", tool_pid.to_str().unwrap()),
            ],
        );

        let survived = stop_recorded(&tool_pid);
        assert_child_done(&output, &done);
        assert!(
            survived,
            "the fake tool did not outlive the SIGKILL, so the fixture tests nothing"
        );
    }

    const KEPT_BACKUP_CHILD: &str =
        "installer::install::tests::a_kept_backup_is_repaired_once_the_timed_out_tool_is_gone";

    /// A fake `kpackagetool6` that starts writing the component, then leaves a
    /// writer in a session of its own holding the pipes, and ends on
    /// `SIGTERM`.
    const SESSION_LEAVING_KPACKAGETOOL: &str = r#"#!/bin/sh
printf half > "$PU_TEST_COMPONENT/marker"
setsid sh -c 'echo $$ > "$PU_TEST_TOOL_PID"; exec sleep 30' &
exec sleep 30
"#;

    /// The first run's timed-out tool leaves a writer behind, so the backup
    /// and its install record stay; once that writer is gone, the next run's
    /// repair puts the component back from the kept backup.
    #[test]
    fn a_kept_backup_is_repaired_once_the_timed_out_tool_is_gone() {
        if let Some(run) = std::env::var_os("PU_TEST_KEPT_BACKUP_RUN") {
            let sites = super::super::RecordSites::scan(false);
            if run == "repair" {
                super::super::repair_records(&sites);
            } else {
                let component = widget_at(
                    Path::new(&std::env::var("PU_TEST_COMPONENT").unwrap()),
                    "org.test.widget",
                );
                let update = crate::types::AvailableUpdate::builder(component, 1, "2.0")
                    .download_url(std::env::var("PU_TEST_DOWNLOAD").unwrap())
                    .build();
                let slot = super::super::InstallSlot::new();
                let refusal = super::super::update_component(
                    &update,
                    &reqwest::blocking::Client::new(),
                    &(),
                    &super::super::BackupLedger::default(),
                    &slot,
                    &sites,
                )
                .err()
                .expect("the timed-out install was reported as landed");
                assert_eq!(refusal.kind, FailureKind::Install);
                assert!(
                    refusal.message.starts_with(
                        "install failed (kpackagetool6 timed out after 1 s and is still running), and the tool that failed it may still be running; the backup was kept at "
                    ),
                    "{}",
                    refusal.message
                );
            }
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("kpackagetool6"), SESSION_LEAVING_KPACKAGETOOL);
        let component = root.join("data/plasma/plasmoids/org.test.widget");
        std::fs::create_dir_all(&component).unwrap();
        std::fs::write(
            component.join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget", "Version": "1.0"}}"#,
        )
        .unwrap();
        std::fs::write(component.join("marker"), b"old").unwrap();
        let package = root.join("src/org.test.widget");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget", "Version": "2.0"}}"#,
        )
        .unwrap();
        let archive = root.join("package.tar");
        let status = std::process::Command::new("tar")
            .arg("--create")
            .arg("--format=ustar")
            .arg("--file")
            .arg(&archive)
            .arg("--directory")
            .arg(root.join("src"))
            .arg("org.test.widget")
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the fixture");
        let body = String::from_utf8(std::fs::read(&archive).unwrap()).unwrap();
        let (base, _) = crate::testutil::serve(move |_| crate::testutil::ok(body.clone()));
        for sub in ["home", "config", "cache", "share", "tmp"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let tool_pid = root.join("tool-pid");
        let done = root.join("child-done");
        let path = |sub: &str| root.join(sub).to_str().unwrap().to_string();
        let download = format!("{base}/package.tar");
        let mut envs = vec![
            ("PU_TEST_KPACKAGETOOL_TIMEOUT_MS", "1000".to_string()),
            ("PU_TEST_CHILD_DONE", done.to_str().unwrap().to_string()),
            ("PU_TEST_COMPONENT", component.to_str().unwrap().to_string()),
            ("PU_TEST_DOWNLOAD", download),
            ("PU_TEST_TOOL_PID", tool_pid.to_str().unwrap().to_string()),
            ("HOME", path("home")),
            ("XDG_DATA_HOME", path("data")),
            ("XDG_CONFIG_HOME", path("config")),
            ("XDG_CACHE_HOME", path("cache")),
            ("XDG_DATA_DIRS", path("share")),
            ("TMPDIR", path("tmp")),
        ];
        let run = |envs: &[(&str, String)]| {
            let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
            run_child(KEPT_BACKUP_CHILD, &bin, &envs)
        };

        envs.push(("PU_TEST_KEPT_BACKUP_RUN", "update".to_string()));
        let timed_out = run(&envs);
        let survived = stop_recorded(&tool_pid);
        assert_child_done(&timed_out, &done);
        assert!(
            survived,
            "the writer did not outlive the run, so the fixture tests nothing"
        );
        assert_eq!(
            std::fs::read_to_string(component.join("marker")).unwrap(),
            "half",
            "the backup was restored over a tree the tool's writer may still be writing"
        );
        let record = super::super::record::record_path(&component);
        let named = std::fs::read_to_string(&record).unwrap();
        let backup = PathBuf::from(named.strip_suffix('\n').unwrap());
        assert_eq!(
            std::fs::read_to_string(backup.join("marker")).unwrap(),
            "old"
        );

        std::fs::remove_file(&done).unwrap();
        envs.pop();
        envs.push(("PU_TEST_KEPT_BACKUP_RUN", "repair".to_string()));
        let repaired = run(&envs);
        assert_child_done(&repaired, &done);
        assert_eq!(
            std::fs::read_to_string(component.join("marker")).unwrap(),
            "old",
            "the next run did not restore the component from the kept backup"
        );
        assert!(
            !record.exists(),
            "the repaired component's install record stayed"
        );
    }

    const FALLBACK_CHILD: &str =
        "installer::install::tests::a_timed_out_fallback_install_does_not_copy_over_the_tool";

    /// A global theme's `kpackagetool6` times out with a writer still holding
    /// the pipes, so the tool may still run: the type's direct fallback must
    /// not copy the package over it, and the backup and its record stay for
    /// the next run.
    #[test]
    fn a_timed_out_fallback_install_does_not_copy_over_the_tool() {
        if std::env::var_os("PU_TEST_FALLBACK_RUN").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let component = InstalledComponent::new(
                "Test Global Theme",
                "org.test.globaltheme",
                ComponentType::GlobalTheme,
                root.join("data/plasma/look-and-feel/org.test.globaltheme"),
            )
            .with_version("1.0");
            let update = crate::types::AvailableUpdate::builder(component, 1, "2.0")
                .download_url(std::env::var("PU_TEST_DOWNLOAD").unwrap())
                .build();
            let slot = super::super::InstallSlot::new();
            let sites = super::super::RecordSites::scan(false);

            let refusal = super::super::update_component(
                &update,
                &reqwest::blocking::Client::new(),
                &(),
                &super::super::BackupLedger::default(),
                &slot,
                &sites,
            )
            .err()
            .expect("a timed-out fallback install was reported as landed");
            assert_eq!(refusal.kind, FailureKind::Install);
            assert!(
                refusal.message.starts_with(
                    "install failed (kpackagetool6 timed out after 1 s and is still running), and the tool that failed it may still be running; the backup was kept at "
                ),
                "{}",
                refusal.message
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("kpackagetool6"), SESSION_LEAVING_KPACKAGETOOL);
        let component_path = root.join("data/plasma/look-and-feel/org.test.globaltheme");
        std::fs::create_dir_all(&component_path).unwrap();
        std::fs::write(
            component_path.join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.globaltheme", "Version": "1.0"}}"#,
        )
        .unwrap();
        std::fs::write(component_path.join("marker"), b"old").unwrap();
        let package = root.join("src/org.test.globaltheme");
        std::fs::create_dir_all(package.join("contents")).unwrap();
        std::fs::write(
            package.join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.globaltheme", "Version": "2.0"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("contents").join("copied"), b"new").unwrap();
        let archive = root.join("package.tar");
        let status = std::process::Command::new("tar")
            .arg("--create")
            .arg("--format=ustar")
            .arg("--file")
            .arg(&archive)
            .arg("--directory")
            .arg(root.join("src"))
            .arg("org.test.globaltheme")
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the fixture");
        let body = String::from_utf8(std::fs::read(&archive).unwrap()).unwrap();
        let (base, _) = crate::testutil::serve(move |_| crate::testutil::ok(body.clone()));
        for sub in ["home", "config", "cache", "share", "tmp"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let tool_pid = root.join("tool-pid");
        let done = root.join("child-done");
        let path = |sub: &str| root.join(sub).to_str().unwrap().to_string();
        let envs = vec![
            ("PU_TEST_FALLBACK_RUN", "1".to_string()),
            ("PU_TEST_KPACKAGETOOL_TIMEOUT_MS", "1000".to_string()),
            ("PU_TEST_CHILD_DONE", done.to_str().unwrap().to_string()),
            ("PU_TEST_ROOT", root.to_str().unwrap().to_string()),
            ("PU_TEST_DOWNLOAD", format!("{base}/package.tar")),
            ("PU_TEST_TOOL_PID", tool_pid.to_str().unwrap().to_string()),
            (
                "PU_TEST_COMPONENT",
                component_path.to_str().unwrap().to_string(),
            ),
            ("HOME", path("home")),
            ("XDG_DATA_HOME", path("data")),
            ("XDG_CONFIG_HOME", path("config")),
            ("XDG_CACHE_HOME", path("cache")),
            ("XDG_DATA_DIRS", path("share")),
            ("TMPDIR", path("tmp")),
        ];
        let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let output = run_child(FALLBACK_CHILD, &bin, &envs);

        let survived = stop_recorded(&tool_pid);
        assert_child_done(&output, &done);
        assert!(
            survived,
            "the tool did not outlive the run, so the fixture tests nothing"
        );
        assert_eq!(
            std::fs::read_to_string(component_path.join("marker")).unwrap(),
            "half",
            "the direct fallback copied over a tool that may still be running"
        );
        assert!(
            !component_path.join("contents").exists(),
            "the direct fallback ran: the package's contents landed in the component"
        );
        let record = super::super::record::record_path(&component_path);
        let named = std::fs::read_to_string(&record).unwrap();
        let backup = PathBuf::from(named.strip_suffix('\n').unwrap());
        assert_eq!(
            std::fs::read_to_string(backup.join("marker")).unwrap(),
            "old",
            "the kept backup changed"
        );
    }

    const ID_LESS_CHILD: &str =
        "installer::install::tests::an_id_less_package_answered_not_installed_is_not_copied";

    /// kpackagetool answers an id-less package "not installed" too (measured:
    /// `Error: Plugin metadata is not installed.`, exit 2), but such a package
    /// is one it refuses to install, so the answer must not lead to a copy.
    #[test]
    fn an_id_less_package_answered_not_installed_is_not_copied() {
        if std::env::var_os("PU_TEST_ID_LESS").is_some() {
            let extract = PathBuf::from(std::env::var("PU_TEST_EXTRACT").unwrap());
            let component_dir = PathBuf::from(std::env::var("PU_TEST_COMPONENT").unwrap());
            // A type with a direct fallback: the package's missing plugin id
            // is judged after the tool answers, since the fallback may still
            // install the package.
            let component = theme_component(
                &component_dir,
                "org.test.widget",
                ComponentType::GlobalTheme,
            );

            let package_dir = prepare_package(&extract, &component, "2.0").unwrap();
            let refusal = install_prepared_package(&package_dir, &component).unwrap_err();

            assert_eq!(refusal.kind, FailureKind::Install);
            assert!(
                !component_dir.join("contents/ui/main.qml").exists(),
                "an id-less package was copied over the component"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin.join("kpackagetool6"),
            "#!/bin/sh\necho 'Error: Plugin metadata is not installed.'\nexit 2\n",
        );
        let package = dir.path().join("extract/org.test.widget");
        std::fs::create_dir_all(package.join("contents/ui")).unwrap();
        std::fs::write(
            package.join("metadata.json"),
            r#"{"KPlugin": {"Name": "Installed Widget", "Version": "2.0"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("contents/ui/main.qml"), "Item {}\n").unwrap();
        let component = dir.path().join("component/org.test.widget");
        std::fs::create_dir_all(&component).unwrap();
        std::fs::write(
            component.join("metadata.json"),
            r#"{"KPlugin": {"Name": "Installed Widget", "Version": "1.0"}}"#,
        )
        .unwrap();
        let done = dir.path().join("child-done");
        let extract = dir.path().join("extract");

        let output = run_child(
            ID_LESS_CHILD,
            &bin,
            &[
                ("PU_TEST_ID_LESS", "1"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_EXTRACT", extract.to_str().unwrap()),
                ("PU_TEST_COMPONENT", component.to_str().unwrap()),
            ],
        );

        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    const NON_OBJECT_CHILD: &str = "installer::install::tests::a_package_metadata_json_that_is_not_an_object_fails_before_kpackagetool";

    #[test]
    fn a_package_metadata_json_that_is_not_an_object_fails_before_kpackagetool() {
        if std::env::var_os("PU_TEST_NON_OBJECT").is_some() {
            let extract = PathBuf::from(std::env::var("PU_TEST_EXTRACT").unwrap());
            let component = widget_at(
                Path::new(&std::env::var("PU_TEST_COMPONENT").unwrap()),
                "org.test.widget",
            );

            let refusal = prepare_package(&extract, &component, "2.0").unwrap_err();

            assert_eq!(refusal.kind, FailureKind::Install);
            assert!(
                !Path::new(&std::env::var("PU_TEST_KPACKAGETOOL_LOG").unwrap()).exists(),
                "kpackagetool6 ran on a metadata.json that is not an object"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dir.path().join("kpackagetool.log");
        write_script(
            &bin.join("kpackagetool6"),
            &format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 0\n", log.display()),
        );
        let extract = dir.path().join("extract");
        std::fs::create_dir_all(extract.join("org.test.widget")).unwrap();
        std::fs::write(
            extract.join("org.test.widget").join("metadata.json"),
            "[1, 2, 3]",
        )
        .unwrap();
        let component = dir.path().join("component");
        let done = dir.path().join("child-done");

        let output = run_child(
            NON_OBJECT_CHILD,
            &bin,
            &[
                ("PU_TEST_NON_OBJECT", "1"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_KPACKAGETOOL_LOG", log.to_str().unwrap()),
                ("PU_TEST_EXTRACT", extract.to_str().unwrap()),
                ("PU_TEST_COMPONENT", component.to_str().unwrap()),
            ],
        );

        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn find_package_dir_skips_a_metadata_json_that_is_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        mkfifo(&dir.path().join("metadata.json"));

        let found = find_package_dir(dir.path());

        assert_eq!(found, None, "a FIFO was taken for the package metadata");
    }

    #[test]
    fn patch_metadata_does_not_block_on_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata.json");
        mkfifo(&path);

        let result =
            within_five_seconds(move || patch_metadata(&path, ComponentType::PlasmaWidget, "2.0"));

        assert!(result.is_err(), "a FIFO was patched as metadata");
    }

    #[test]
    fn find_package_dir_prefers_a_root_metadata_desktop_over_a_nested_metadata_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("metadata.desktop"),
            "[Desktop Entry]\nName=Root\n",
        )
        .unwrap();
        let nested = dir.path().join("contents/config");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("metadata.json"), "{}").unwrap();

        let found = find_package_dir(dir.path());

        assert_eq!(found.as_deref(), Some(dir.path()));
    }

    #[test]
    fn find_package_dir_finds_a_package_one_level_down() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("org.example.widget");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("metadata.json"), "{}").unwrap();

        assert_eq!(find_package_dir(dir.path()), Some(package));
    }

    #[test]
    fn find_package_dir_stops_descending_past_its_depth_bound() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for level in 0..64 {
            deep.push(format!("d{level}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("metadata.json"), "{}").unwrap();

        // No KDE Store package nests its root 64 directories down; a walker
        // that still finds it has no bound.
        assert_eq!(find_package_dir(dir.path()), None);
    }

    const SILENT_KPACKAGETOOL_CHILD: &str =
        "installer::install::tests::a_silent_kpackagetool_failure_names_the_exit_status";

    /// A `kpackagetool6` that exits nonzero printing nothing is reported with
    /// the shared exit-status form, the one `systemctl`, `bsdtar` and `sudo`
    /// use, never the doubled `exited with exit status: 1` an `ExitStatus`
    /// interpolates.
    #[test]
    fn a_silent_kpackagetool_failure_names_the_exit_status() {
        if std::env::var_os("PU_TEST_SILENT_KPACKAGETOOL").is_some() {
            let extract = PathBuf::from(std::env::var("PU_TEST_EXTRACT").unwrap());
            let component = widget_at(
                Path::new(&std::env::var("PU_TEST_COMPONENT").unwrap()),
                "org.test.widget",
            );

            let package_dir = prepare_package(&extract, &component, "2.0").unwrap();
            let refusal = install_prepared_package(&package_dir, &component).unwrap_err();

            assert_eq!(
                refusal.message,
                "kpackagetool6 failed: kpackagetool6 exited with status 1"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("kpackagetool6"), "#!/bin/sh\nexit 1\n");
        let extract = dir.path().join("extract");
        std::fs::create_dir_all(extract.join("org.test.widget")).unwrap();
        std::fs::write(
            extract.join("org.test.widget").join("metadata.json"),
            r#"{"KPlugin": {"Id": "org.test.widget"}}"#,
        )
        .unwrap();
        let component = dir.path().join("component");
        let done = dir.path().join("child-done");

        let output = run_child(
            SILENT_KPACKAGETOOL_CHILD,
            &bin,
            &[
                ("PU_TEST_SILENT_KPACKAGETOOL", "1"),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
                ("PU_TEST_EXTRACT", extract.to_str().unwrap()),
                ("PU_TEST_COMPONENT", component.to_str().unwrap()),
            ],
        );

        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// The arm that returns the unsupported-format reason is unreachable while
    /// only PNG, JPEG and WebP are enabled, so its bytes can only be pinned
    /// here.
    #[test]
    fn the_unsupported_image_reason_keeps_its_wording() {
        assert_eq!(UNSUPPORTED_IMAGE, "not a PNG, JPEG or WebP image");
    }

    #[test]
    fn own_file_name_refuses_a_name_that_is_no_file_name() {
        assert_eq!(
            own_file_name(OsStr::new("forest.png")),
            Some(OsStr::new("forest.png"))
        );
        assert_eq!(
            ImageName::Served(Ok("forest.png")).file_name(),
            Some(OsStr::new("forest.png"))
        );
        assert_eq!(ImageName::Served(Err("%FF.png")).file_name(), None);
        for name in ["", ".", "..", "a/b.png", "a\0b.png", "/"] {
            assert_eq!(own_file_name(OsStr::new(name)), None, "{name:?}");
            let failure = ImageName::Member(OsStr::new(name)).no_file_name();
            assert_eq!(
                failure.kind,
                FailureKind::Install,
                "{name:?} failed with the wrong kind"
            );
            assert_eq!(
                failure.message,
                format!(
                    "downloaded image has no file name to put in the wallpaper folder ({name:?})"
                ),
                "{name:?}"
            );
        }
        assert_eq!(
            ImageName::Served(Err("%FF.png")).no_file_name().message,
            "downloaded image has no file name to put in the wallpaper folder (\"%FF.png\")"
        );
    }
}
