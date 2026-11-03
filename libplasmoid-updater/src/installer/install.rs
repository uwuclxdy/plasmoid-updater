// SPDX-License-Identifier: GPL-3.0-or-later
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    collections::BTreeSet,
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

/// The display names of the components a killed run stranded in their
/// `.plasmoid-updater-old` sibling with no visible component beside them.
///
/// A `-new` sibling alone is a half-copy that never held the component, and one
/// beside its component is a dead copy: neither is a component a check names.
pub(crate) fn stranded_components(system: bool) -> Vec<String> {
    recover_dirs(system)
        .iter()
        .flat_map(|dir| stranded_in_dir(dir))
        .collect()
}

/// The stranded components of one directory, named for display.
fn stranded_in_dir(dir: &Path) -> Vec<String> {
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
            stranded.push(stranded_display_name(&old, &component));
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

/// How long a killed `kpackagetool6` and its process group get to exit before
/// they are killed outright, and how long its pipe readers may take to finish.
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

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
/// `--global` as needed.
///
/// The not-installed answer this module matches is English text and
/// kpackagetool prints it in the locale it inherits, so every call runs in the
/// C locale. `sudo` resets the environment, so there the locale goes through
/// `env`, which `sudo` passes on to the tool.
fn kpackagetool_cmd(kpackage_type: &str, global: bool) -> std::process::Command {
    let mut cmd = if global && !privilege::is_root() {
        let mut cmd = privilege::sudo_command("env");
        cmd.args(["LC_ALL=C", "LANGUAGE=C", "kpackagetool6"]);
        cmd
    } else {
        let mut cmd = std::process::Command::new("kpackagetool6");
        cmd.env("LC_ALL", "C").env("LANGUAGE", "C");
        cmd
    };
    cmd.arg("-t").arg(kpackage_type);
    if global {
        cmd.arg("--global");
    }
    cmd
}

/// Drains one of a child's pipes on its own thread.
fn spawn_reader<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

/// Joins the pipe readers, giving them `grace` to finish.
///
/// A reader ends when every holder of its write end dies. A process that
/// outlived the kill keeps one open, so a plain join could hold the run; this
/// returns `false` instead.
fn join_readers_bounded(
    readers: Vec<std::thread::JoinHandle<Vec<u8>>>,
    grace: std::time::Duration,
) -> (Vec<Vec<u8>>, bool) {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if readers.iter().all(std::thread::JoinHandle::is_finished) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return (Vec::new(), false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let drained = readers
        .into_iter()
        .map(|reader| reader.join().unwrap_or_default())
        .collect();
    (drained, true)
}

/// Whether any process is left in `group`, checked without sending a signal.
fn process_group_alive(group: Pid) -> bool {
    killpg(group, None).is_ok()
}

/// Ends the child and every process it forked, giving the group a grace on
/// `SIGTERM` before `SIGKILL`: `sudo` and the tool under it share the group,
/// so the signal reaches the tool that actually holds the pipes. Returns
/// whether a process survived the kill, which only a process running as
/// another user (a root tool under sudo) can.
fn terminate_group(child: &mut std::process::Child, group: Pid) -> bool {
    let _ = killpg(group, Signal::SIGTERM);
    let grace_deadline = std::time::Instant::now() + KILL_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return process_group_alive(group),
            Ok(None) if std::time::Instant::now() >= grace_deadline => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(_) => return process_group_alive(group),
        }
    }
    let _ = killpg(group, Signal::SIGKILL);
    let _ = child.wait();
    process_group_alive(group)
}

/// Runs `command` in its own process group, killing the whole group once
/// `timeout` has passed.
///
/// Stdout and stderr are drained while the tool runs, so a child that fills a
/// pipe cannot block the wait; a killed group closes its pipes and both
/// readers finish. The readers are joined with a bound, because a descendant
/// that survived the grace would otherwise keep the run waiting.
fn run_with_timeout(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    tool: &str,
) -> Result<std::process::Output, ComponentFailure> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| install_failure(format!("failed to run {tool}: {e}")))?;
    let group = Pid::from_raw(child.id() as i32);
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
                let (drained, finished) = join_readers_bounded(readers, KILL_GRACE);
                if !finished {
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
                let still_running = terminate_group(&mut child, group);
                let _ = join_readers_bounded(readers, KILL_GRACE);
                let message = if still_running {
                    format!(
                        "{tool} timed out after {} s and is still running",
                        timeout.as_secs()
                    )
                } else {
                    format!("{tool} timed out after {} s", timeout.as_secs())
                };
                let mut failure = install_failure(message);
                failure.process_may_still_run = still_running;
                return Err(failure);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => {
                let still_running = terminate_group(&mut child, group);
                let _ = join_readers_bounded(readers, KILL_GRACE);
                let mut failure = install_failure(format!("failed to wait for {tool}: {e}"));
                failure.process_may_still_run = still_running;
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

    let output = run_with_timeout(
        kpackagetool_cmd(kpackage_type, global)
            .arg("-u")
            .arg(package_dir),
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

/// Installs or updates a component using kpackagetool, with metadata patching.
///
/// kpackagetool addresses an installed package by its plugin id, so a
/// component whose directory is named differently is installed at its own
/// path instead: `-u` would answer "not installed" and `-i` would add a
/// second tree under the id, leaving this one stale.
pub(super) fn install_via_kpackage(
    extract_dir: &Path,
    component: &InstalledComponent,
    new_version: &str,
) -> Result<(), ComponentFailure> {
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

    if let Some(package_id) = plugin_id(&package_dir)
        && package_id != component.directory_name
    {
        log::info!(
            target: "install",
            "installing {} directly: its directory name differs from the plugin id {:?}",
            component.name,
            package_id,
        );
        return atomic_install_dir(&package_dir, &component.path);
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

// --- Direct Installation Methods ---

/// Installs a component using direct file operations (not kpackagetool).
pub(super) fn install_direct(
    extract_dir: &Path,
    component: &InstalledComponent,
) -> Result<(), ComponentFailure> {
    match component.component_type {
        ComponentType::ColorScheme => install_color_scheme(extract_dir, &component.path),
        ComponentType::IconTheme | ComponentType::CursorTheme => {
            install_index_theme(extract_dir, &component.path, &component.directory_name)
        }
        ComponentType::Wallpaper => install_wallpaper(extract_dir, component),
        ComponentType::AuroraeDecoration
        | ComponentType::GlobalTheme
        | ComponentType::PlasmaStyle
        | ComponentType::SplashScreen
        | ComponentType::SddmTheme => {
            install_theme_dir(extract_dir, &component.path, component.component_type)
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

fn install_color_scheme(extract_dir: &Path, dest_path: &Path) -> Result<(), ComponentFailure> {
    let color_file = locate_color_scheme_file(extract_dir)
        .ok_or_else(|| install_failure("no color scheme file found in archive"))?;

    atomic_install_file(&color_file, dest_path)?;
    log::debug!(target: "install", "copied color scheme to {}", dest_path.display());
    Ok(())
}

/// Installs the directory holding `index.theme` (an icon or cursor theme).
///
/// A multi-variant pack ships several theme dirs in one archive; the variant
/// named like the component is the one it owns. With no same-named variant, a
/// single-theme archive still installs as before, and several variants none of
/// which match is an error rather than a guess.
fn install_index_theme(
    extract_dir: &Path,
    dest_dir: &Path,
    directory_name: &str,
) -> Result<(), ComponentFailure> {
    let themes = find_index_theme_dirs(extract_dir);
    if themes.is_empty() {
        return Err(install_failure("no theme (index.theme) found in archive"));
    }
    let source_dir = themes
        .iter()
        .map(PathBuf::as_path)
        .find(|dir| dir.file_name().is_some_and(|name| name == directory_name))
        .or_else(|| (themes.len() == 1).then(|| themes[0].as_path()))
        .ok_or_else(|| {
            // Sorted: directory iteration order is not stable, and the message
            // names the variants in a fixed order.
            let mut variants: Vec<String> = themes
                .iter()
                .filter_map(|dir| dir.file_name().and_then(|name| name.to_str()))
                .map(|name| format!("{name:?}"))
                .collect();
            variants.sort();
            install_failure(format!(
                "archive holds several themes ({}) and none is named {directory_name:?}",
                variants.join(", ")
            ))
        })?;

    atomic_install_dir(source_dir, dest_dir)?;
    log::debug!(target: "install", "copied theme to {}", dest_dir.display());
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
        // install pool and kill the whole run, is not.
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

            install_direct(&extract, &component).unwrap();

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

        let failure = install_direct(&extract, &component).unwrap_err();

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

        let failure = install_direct(&extract, &component).unwrap_err();

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

        install_direct(&extract, &component).unwrap();

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
    }

    #[test]
    fn the_process_group_probe_sees_a_live_group_and_a_dead_one() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Pid::from_raw(child.id() as i32);

        assert!(process_group_alive(group), "a live group was read as gone");

        let _ = killpg(group, Signal::SIGKILL);
        let _ = child.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while process_group_alive(group) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            !process_group_alive(group),
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

            let refusal = install_via_kpackage(&extract, &component, "2.0").unwrap_err();

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
            let component = widget_at(&component_dir, "org.test.widget");

            let refusal = install_via_kpackage(&extract, &component, "2.0").unwrap_err();

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

            let refusal = install_via_kpackage(&extract, &component, "2.0").unwrap_err();

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

            let refusal = install_via_kpackage(&extract, &component, "2.0").unwrap_err();

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
}
