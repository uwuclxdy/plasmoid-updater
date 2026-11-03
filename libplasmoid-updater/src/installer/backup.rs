// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

use super::ComponentFailure;
use crate::{FailureKind, installer::privilege, types::InstalledComponent};

const MAX_BACKUPS_PER_TYPE: usize = 5;

/// The mode every root-owned system backup directory is created with: root
/// owns it and only root writes it, while the invoking user keeps search
/// permission so a restore can tell a file backup from a directory one.
const SYSTEM_BACKUP_DIR_MODE: u32 = 0o755;

/// One backup this run made, so retention can leave it alone.
struct BackupRecord {
    timestamp_dir: PathBuf,
    type_subdir: &'static str,
    /// A `--system` backup: written and pruned through sudo, under the
    /// root-owned base.
    system: bool,
}

/// The timestamp directories one run's backups land in, so retention can leave
/// every one of them alone.
#[derive(Default)]
pub(crate) struct BackupLedger {
    created: std::sync::Mutex<Vec<BackupRecord>>,
}

impl BackupLedger {
    fn record(&self, timestamp_dir: &Path, type_subdir: &'static str, system: bool) {
        let mut created = self
            .created
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        created.push(BackupRecord {
            timestamp_dir: timestamp_dir.to_path_buf(),
            type_subdir,
            system,
        });
    }
}

/// Keeps the newest [`MAX_BACKUPS_PER_TYPE`] backups of every component type
/// this run backed up, never taking away one this run made.
///
/// Called once the run's installs have all finished: pruning inside one
/// component's backup could take away a backup a still-running component of the
/// same type will need for its restore.
pub(crate) fn prune_backups(ledger: &BackupLedger) {
    let created = {
        let mut guard = ledger
            .created
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *guard)
    };
    if created.is_empty() {
        return;
    }
    // One run can write both bases; each one is listed and pruned the way its
    // backups were written.
    let mut by_base: BTreeMap<(PathBuf, bool), Vec<BackupRecord>> = BTreeMap::new();
    for record in created {
        if let Some(base) = record.timestamp_dir.parent() {
            by_base
                .entry((base.to_path_buf(), record.system))
                .or_default()
                .push(record);
        }
    }
    for ((base, system), records) in by_base {
        prune_backups_in(&base, &records, system, MAX_BACKUPS_PER_TYPE);
    }
}

fn prune_backups_in(base: &Path, created: &[BackupRecord], system: bool, max_keep: usize) {
    let types: BTreeSet<&str> = created.iter().map(|record| record.type_subdir).collect();
    let skip: Vec<PathBuf> = created
        .iter()
        .map(|record| record.timestamp_dir.clone())
        .collect();
    for type_subdir in types {
        cleanup_old_backups_in(base, type_subdir, max_keep, &skip, system);
    }
}

/// The base backup directory for user-mode backups.
fn user_backup_base_dir() -> PathBuf {
    crate::paths::cache_home().join("plasmoid-updater/backups")
}

/// The base backup directory for `--system` backups: root-owned, reached only
/// through sudo.
pub(crate) fn system_backup_base_dir() -> PathBuf {
    PathBuf::from("/var/lib/plasmoid-updater/backups")
}

fn backup_base_dir_for(component: &InstalledComponent, system_base: &Path) -> PathBuf {
    if component.is_system {
        system_base.to_path_buf()
    } else {
        user_backup_base_dir()
    }
}

/// Generates a timestamp string for backup directories.
fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string()
}

/// Creates a backup of the component before updating, recording the timestamp
/// directory it lands in so this run's retention leaves it alone.
/// Returns the path to the backup, or `None` if the component path
/// does not exist on disk (nothing to back up).
pub(crate) fn backup_component(
    component: &InstalledComponent,
    backups: &BackupLedger,
) -> Result<Option<PathBuf>, ComponentFailure> {
    backup_component_at(
        component,
        &system_backup_base_dir(),
        backups,
        crate::paths::is_trusted_system_path,
    )
}

/// Seam: `system_base` names the root system backups land under and `trusted`
/// the run's system-path check, so a test can inject a tempdir root and a
/// trust verdict the real check would never give it.
fn backup_component_at(
    component: &InstalledComponent,
    system_base: &Path,
    backups: &BackupLedger,
    trusted: impl Fn(&Path) -> bool,
) -> Result<Option<PathBuf>, ComponentFailure> {
    let base = backup_base_dir_for(component, system_base);
    if component.is_system {
        ensure_system_backup_base(&base, trusted)?;
    }
    let backup = backup_component_in(&base, component)?;
    if let Some(backup) = &backup
        && let Some(timestamp_dir) = backup.parent().and_then(Path::parent)
    {
        backups.record(
            timestamp_dir,
            component.component_type.backup_subdir(),
            component.is_system,
        );
    }
    Ok(backup)
}

/// Creates the root-owned system backups base with its own mode, then checks
/// the base this run is about to write into: a path that is not, or does not
/// sit under, a root-owned directory only root can write is refused by name,
/// so a swapped base can never receive a backup.
fn ensure_system_backup_base(
    base: &Path,
    trusted: impl Fn(&Path) -> bool,
) -> Result<(), ComponentFailure> {
    // Every level that already exists is checked before root creates anything
    // under it: search permission first, then trust. A base or ancestor the
    // invoking user cannot search (a root-owned 0700/0750 an older run or an
    // admin left) is refused by name with its fix, never created over and never
    // left to fail the restore later; a swapped or symlinked ancestor is
    // refused rather than followed.
    if let Some(level) = unsearchable_level(base) {
        return Err(unsearchable_failure(base, level));
    }
    if base.try_exists().unwrap_or(false) {
        if trusted(base) {
            return Ok(());
        }
        return Err(untrusted_base_failure(base));
    }
    // A missing base under an existing ancestor: the ancestor is checked before
    // root's mkdir, so a symlinked or foreign ancestor is refused rather than
    // followed.
    if let Some(level) = base.ancestors().find(|level| level.exists())
        && !trusted(level)
    {
        return Err(untrusted_base_failure(base));
    }
    privilege::create_dir_all_as_root(base, SYSTEM_BACKUP_DIR_MODE)?;
    if trusted(base) {
        return Ok(());
    }
    Err(untrusted_base_failure(base))
}

/// The shallowest existing directory on the chain up to `base` the invoking
/// user cannot search, if any. A root-owned 0700/0750 level passes the trust check
/// (which refuses group and other WRITE only) yet the invoking user still
/// cannot search it, and the user-side restore probe would read a file backup
/// as a directory. Walked shallowest first, so the first level found is the one
/// `sudo chmod 755` must fix. A level the user owns and can search (a 0700 one
/// they made themselves) is not refused: the search check answers for the
/// invoking user, not only the mode bits.
fn unsearchable_level(base: &Path) -> Option<&Path> {
    use nix::unistd::AccessFlags;

    let mut levels: Vec<&Path> = base
        .ancestors()
        .filter(|level| level.parent().is_some())
        .collect();
    levels.reverse();
    levels.into_iter().find(|&level| {
        nix::unistd::access(level, AccessFlags::X_OK) == Err(nix::errno::Errno::EACCES)
    })
}

/// The refusal for a system backup path level the invoking user cannot search,
/// naming the level and the fix to paste when the path is valid UTF-8.
fn unsearchable_failure(base: &Path, level: &Path) -> ComponentFailure {
    match level.to_str() {
        Some(level) => ComponentFailure::new(
            FailureKind::Install,
            format!(
                "refusing to write a system backup into {}: {level} is not searchable by this user: sudo chmod 755 {}",
                base.display(),
                shell_quote(level),
            ),
        ),
        // A non-UTF-8 path cannot be shell-quoted losslessly, so the hint names
        // the path instead of a pasteable command that would run against a
        // different directory.
        None => ComponentFailure::new(
            FailureKind::Install,
            format!(
                "refusing to write a system backup into {}: {level:?} is not searchable by this user",
                base.display(),
            ),
        ),
    }
}

/// The refusal for a system backups base the trust check did not accept.
fn untrusted_base_failure(base: &Path) -> ComponentFailure {
    ComponentFailure::new(
        FailureKind::Install,
        format!(
            "refusing to write a system backup into {}: it, or a directory above it, is a symlink, not owned by root, or writable by group or others",
            base.display()
        ),
    )
}

fn backup_component_in(
    base: &Path,
    component: &InstalledComponent,
) -> Result<Option<PathBuf>, ComponentFailure> {
    match component.path.try_exists() {
        Ok(true) => {}
        Ok(false) => {
            log::debug!(
                target: "backup",
                "nothing to back up for {}: {}",
                component.name,
                component.path.display(),
            );
            return Ok(None);
        }
        // An unreadable path is not an absent one: backing up nothing here
        // would let the update overwrite a component no restore can bring back.
        Err(e) => {
            return Err(ComponentFailure::io(
                FailureKind::Install,
                &component.path,
                e,
            ));
        }
    }

    let system = component.is_system;
    let timestamp = timestamp();
    let type_dir = component.component_type.backup_subdir();
    let timestamp_dir = base.join(&timestamp);

    // handle single files (e.g., color schemes, static wallpapers)
    if component.path.is_file() {
        let backup_dir = timestamp_dir.join(type_dir);
        create_backup_dir(&backup_dir, system)?;

        let backup_path = backup_dir.join(&component.directory_name);
        if let Err(e) = copy_backup_file(&component.path, &backup_path, system) {
            // A failed copy may leave a partial file; a removal that fails
            // leaves a file a restore could pick up, so it cannot be dropped.
            match remove_backup_file(&backup_path, system) {
                Ok(()) => {}
                Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => {}
                Err(remove_error) => log::warn!(
                    target: "backup",
                    "failed to remove partial backup {}: {remove_error}",
                    backup_path.display()
                ),
            }
            remove_empty_backup_dirs(&backup_path, system);
            return Err(e);
        }

        return Ok(Some(backup_path));
    }

    let backup_path = timestamp_dir.join(type_dir).join(&component.directory_name);

    create_backup_dir(&backup_path, system)?;

    if let Err(e) = copy_backup_dir(&component.path, &backup_path, system) {
        remove_partial_backup(&backup_path, system);
        return Err(e);
    }

    Ok(Some(backup_path))
}

/// Creates a backup directory. A refusal on a user-mode directory names the
/// chown fix; a `--system` directory is created through sudo.
fn create_backup_dir(path: &Path, system: bool) -> Result<(), ComponentFailure> {
    if system {
        privilege::create_dir_all_as_root(path, SYSTEM_BACKUP_DIR_MODE)
    } else {
        std::fs::create_dir_all(path).map_err(|e| backup_io_failure(path, e))
    }
}

/// The failure a user-mode backup step reports. Only a permission refusal of
/// the backups directory names the chown fix; every other failure (a full
/// disk, a bad path) keeps the operating system's own message.
fn backup_io_failure(path: &Path, source: std::io::Error) -> ComponentFailure {
    if source.kind() == std::io::ErrorKind::PermissionDenied {
        not_writable_failure(path, Some(&source))
    } else {
        ComponentFailure::io(FailureKind::Install, path, source)
    }
}

fn copy_backup_file(src: &Path, dst: &Path, system: bool) -> Result<(), ComponentFailure> {
    if system {
        privilege::copy_file_as_root(src, dst)
    } else {
        std::fs::copy(src, dst).map_err(|e| match dst.parent() {
            // A same-second retry writes into a directory an earlier run left
            // root-owned: the directory names the fix, the bare OS error would
            // not distinguish it from an unreadable component.
            Some(dir)
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    && !super::lock::writable_by_current_ids(dir) =>
            {
                not_writable_failure(dir, Some(&e))
            }
            _ => ComponentFailure::io(FailureKind::Install, src, e),
        })?;
        Ok(())
    }
}

fn copy_backup_dir(src: &Path, dst: &Path, system: bool) -> Result<(), ComponentFailure> {
    if system {
        privilege::copy_dir_as_root(src, dst)
    } else {
        copy_dir_recursive(src, dst).map_err(|failure| {
            // The hint fires only on a permission refusal of the destination
            // directory, and keeps the operating system's error, the way the
            // single-file copy does. A disk-full or unreadable source refusal
            // keeps its own message.
            match &failure.source {
                Some(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        && !super::lock::writable_by_current_ids(dst) =>
                {
                    not_writable_failure(dst, failure.source.as_ref())
                }
                _ => failure,
            }
        })
    }
}

/// The chown fix a user-mode backup step reports when the backups directory
/// itself refused the write, with the operating system's own error when one
/// names a different cause.
fn not_writable_failure(dir: &Path, source: Option<&std::io::Error>) -> ComponentFailure {
    let cache_dir = crate::paths::cache_home().join("plasmoid-updater");
    ComponentFailure::new(FailureKind::Install, chown_hint(dir, source, &cache_dir))
}

/// The full chown-hint message: the directory that refused the write, the
/// operating system's own error when one names a different cause, and the
/// command to paste, whose path is shell-quoted.
fn chown_hint(dir: &Path, source: Option<&std::io::Error>, cache_dir: &Path) -> String {
    let reason = source.map_or_else(String::new, |e| format!(": {e}"));
    match cache_dir.to_str() {
        Some(cache) => format!(
            "{}{reason}; {} is not writable by this user: sudo chown -R \"$USER\": {}",
            dir.display(),
            cache_dir.display(),
            shell_quote(cache),
        ),
        // A non-UTF-8 path cannot be shell-quoted losslessly, so the hint names
        // the path instead of a pasteable command that would run against a
        // different directory.
        None => format!(
            "{}{reason}; {:?} is not writable by this user",
            dir.display(),
            cache_dir,
        ),
    }
}

/// Single-quotes `path` for the shell command the hint asks the user to paste:
/// a `'` becomes `'\''`, so a path holding a space or a quote survives.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

fn remove_backup_file(path: &Path, system: bool) -> std::io::Result<()> {
    if system {
        privilege::remove_file_as_root(path)
            .map_err(|failure| std::io::Error::other(failure.message))
    } else {
        std::fs::remove_file(path)
    }
}

/// Removes a backup left half-written by a failed copy, so a later restore
/// cannot pick it up as a whole tree.
fn remove_partial_backup(backup_path: &Path, system: bool) {
    let removal = if system {
        privilege::remove_dir_all_as_root(backup_path)
    } else {
        std::fs::remove_dir_all(backup_path)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, backup_path, e))
    };
    if let Err(e) = removal {
        log::warn!(
            target: "backup",
            "failed to remove partial backup {}: {}",
            backup_path.display(),
            e.message
        );
        return;
    }
    remove_empty_backup_dirs(backup_path, system);
}

/// Removes the timestamp and type directories a failed backup left empty, so no
/// empty directory counts as a backup of the component's type.
fn remove_empty_backup_dirs(backup_path: &Path, system: bool) {
    if let Some(type_dir) = backup_path.parent() {
        let _ = remove_dir_or_sudo(type_dir, system);
        if let Some(timestamp_dir) = type_dir.parent() {
            let _ = remove_dir_or_sudo(timestamp_dir, system);
        }
    }
}

fn remove_dir_or_sudo(path: &Path, system: bool) -> Result<(), ComponentFailure> {
    if system {
        privilege::remove_dir_as_root(path)
    } else {
        std::fs::remove_dir(path).map_err(|e| ComponentFailure::io(FailureKind::Install, path, e))
    }
}

/// Restores a component from its backup.
///
/// A directory is swapped in with one atomic exchange where the platform offers
/// it — `renameat2(RENAME_EXCHANGE)` on a filesystem that supports it, or
/// `mv --exchange` under sudo for a system path — so the original path holds
/// either the backup tree or the tree it replaces at every moment. Otherwise the
/// two-rename fallback runs, which has a brief window where the original path is
/// absent and puts the old tree back if its second rename fails. A file is
/// replaced by an atomic rename. A failed copy leaves the original path
/// untouched.
pub(crate) fn restore_component(
    backup_path: &Path,
    original_path: &Path,
) -> Result<(), ComponentFailure> {
    use super::install::{atomic_install_dir, atomic_install_file};

    if backup_path.is_file() {
        atomic_install_file(backup_path, original_path).map_err(|e| {
            ComponentFailure::new(
                FailureKind::RestoreFailed,
                format!("restore file: {}", e.message),
            )
        })?;
    } else {
        atomic_install_dir(backup_path, original_path).map_err(|e| {
            ComponentFailure::new(
                FailureKind::RestoreFailed,
                format!("restore dir: {}", e.message),
            )
        })?;
    }

    Ok(())
}

pub(super) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), ComponentFailure> {
    if !src.is_dir() {
        return Err(ComponentFailure::new(
            FailureKind::Install,
            format!("source is not a directory: {}", src.display()),
        ));
    }

    copy_tree(src, dst)
}

/// Copies the contents of `src` into `dst`, reporting the path that failed and
/// the operating system's own error for it.
///
/// A symlink is recreated as a link rather than followed, so a link inside a
/// package cannot pull in content from outside it. An entry that is neither a
/// file, a directory nor a link (a FIFO, a socket, a device node) is refused
/// before it is opened: opening a FIFO for reading waits for a writer that may
/// never come. A directory's permission bits are applied after its contents are
/// copied, so a directory the copy may not write into does not stop its own
/// copy, and the owner keeps write access whatever the package shipped:
/// unlinking an entry needs write permission on the directory holding it, so a
/// copied 0555 directory would make the install's own replaced tree, and every
/// later temp copy of it, undeletable. The top-level `src` may itself be a link
/// to the directory (a component's path can be one): its mode is read through
/// the link, the way `cp -r` copies the target.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), ComponentFailure> {
    let metadata =
        fs::metadata(src).map_err(|e| ComponentFailure::io(FailureKind::Install, src, e))?;
    fs::create_dir_all(dst).map_err(|e| ComponentFailure::io(FailureKind::Install, dst, e))?;

    let entries =
        fs::read_dir(src).map_err(|e| ComponentFailure::io(FailureKind::Install, src, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| ComponentFailure::io(FailureKind::Install, src, e))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| ComponentFailure::io(FailureKind::Install, &from, e))?;
        if file_type.is_symlink() {
            let target = fs::read_link(&from)
                .map_err(|e| ComponentFailure::io(FailureKind::Install, &from, e))?;
            std::os::unix::fs::symlink(&target, &to)
                .map_err(|e| ComponentFailure::io(FailureKind::Install, &to, e))?;
        } else if file_type.is_dir() {
            copy_tree(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)
                .map_err(|e| ComponentFailure::io(FailureKind::Install, &from, e))?;
        } else {
            return Err(ComponentFailure::io(
                FailureKind::Install,
                &from,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "not a regular file or directory",
                ),
            ));
        }
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(dst, permissions)
        .map_err(|e| ComponentFailure::io(FailureKind::Install, dst, e))?;
    Ok(())
}

/// The timestamp [`timestamp`] encodes in a backup directory's name.
const BACKUP_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";

/// Orders backups oldest first: by the instant the name encodes, and a shared
/// instant by name, so the order never follows the order the directory was
/// listed in.
fn oldest_first(backups: &mut [(chrono::NaiveDateTime, PathBuf)]) {
    backups.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
}

/// The timestamp directories under a user backups base that hold a backup of
/// `type_subdir`.
fn user_backup_dirs(base: &Path, type_subdir: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join(type_subdir).is_dir())
        .collect()
}

/// The timestamp directories under a system backups base that hold a backup
/// of `type_subdir`, listed as root: the base is root-owned, and a `0700` one
/// is not listable by the user the run acts as.
fn system_backup_dirs(base: &Path, type_subdir: &str) -> Vec<PathBuf> {
    if privilege::is_root() {
        return user_backup_dirs(base, type_subdir);
    }
    let listed = privilege::run_sudo_reading_os(&[
        OsStr::new("find"),
        base.as_os_str(),
        OsStr::new("-mindepth"),
        OsStr::new("2"),
        OsStr::new("-maxdepth"),
        OsStr::new("2"),
        OsStr::new("-type"),
        OsStr::new("d"),
        OsStr::new("-name"),
        OsStr::new(type_subdir),
        OsStr::new("-print"),
    ]);
    match listed {
        Ok(output) => output
            .lines()
            .filter_map(|line| Path::new(line).parent().map(Path::to_path_buf))
            .collect(),
        Err(e) => {
            log::warn!(
                target: "backup",
                "could not list the system backups under {}: {}",
                base.display(),
                e.message
            );
            Vec::new()
        }
    }
}

/// Removes old backup directories, keeping the most recent `max_keep` and
/// never one of `skip`, the directories the current run created.
///
/// Only a directory whose name parses as a timestamp is a backup, and only a
/// backup is ever pruned: a stray directory is neither counted toward the limit
/// nor removed in a real backup's place. The encoded timestamp orders the
/// backups, since a cache restored from a tarball or written on another clock
/// carries mtimes that say nothing about when the backups were taken.
fn cleanup_old_backups_in(
    base: &Path,
    type_subdir: &str,
    max_keep: usize,
    skip: &[PathBuf],
    system: bool,
) {
    let candidates = if system {
        system_backup_dirs(base, type_subdir)
    } else {
        user_backup_dirs(base, type_subdir)
    };

    let mut backups: Vec<(chrono::NaiveDateTime, PathBuf)> = candidates
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let created =
                chrono::NaiveDateTime::parse_from_str(name, BACKUP_TIMESTAMP_FORMAT).ok()?;
            Some((created, path))
        })
        .collect();

    let mut to_remove = backups.len().saturating_sub(max_keep);
    if to_remove == 0 {
        return;
    }

    oldest_first(&mut backups);

    for (_, path) in backups {
        if to_remove == 0 {
            return;
        }
        // This run's own backups are chosen by identity: clock skew can sort
        // one oldest, and restoring an update's own backup is the one thing
        // retention must never take away.
        if skip.contains(&path) {
            continue;
        }
        remove_backup(&path, type_subdir, system);
        to_remove -= 1;
    }
}

fn remove_backup(path: &Path, type_subdir: &str, system: bool) {
    let type_path = path.join(type_subdir);
    let removal = if system {
        privilege::remove_dir_all_as_root(&type_path)
    } else {
        std::fs::remove_dir_all(&type_path)
            .map_err(|e| ComponentFailure::io(FailureKind::Install, &type_path, e))
    };
    if let Err(e) = removal {
        log::warn!(
            target: "backup",
            "failed to remove old backup {}: {}",
            type_path.display(),
            e.message
        );
    }
    // Remove the timestamp dir too if it's now empty. Through sudo, `rmdir`
    // itself refuses a directory another type's backup still fills.
    if system {
        let _ = remove_dir_or_sudo(path, true);
    } else if path.read_dir().map_or(true, |mut d| d.next().is_none()) {
        let _ = remove_dir_or_sudo(path, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::logs::{capture_logs, captured_logs};
    use crate::types::ComponentType;

    #[test]
    fn cleanup_old_backups_keeps_recent() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();

        // Create 7 fake backup dirs
        for i in 1..=7 {
            let ts_dir = base.path().join(format!("2024-01-0{i}T00-00-00"));
            let type_dir = ts_dir.join(subdir);
            std::fs::create_dir_all(&type_dir).unwrap();
            std::fs::write(type_dir.join("dummy"), b"data").unwrap();
        }

        // Verify 7 exist
        let count_before = std::fs::read_dir(base.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(subdir).exists())
            .count();
        assert_eq!(count_before, 7);

        // Run cleanup keeping 5
        cleanup_old_backups_in(base.path(), subdir, 5, &[], false);

        // Verify only 5 remain
        let count_after = std::fs::read_dir(base.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(subdir).exists())
            .count();
        assert_eq!(count_after, 5);

        // Verify the oldest 2 were removed (01 and 02)
        assert!(
            !base
                .path()
                .join("2024-01-01T00-00-00")
                .join(subdir)
                .exists()
        );
        assert!(
            !base
                .path()
                .join("2024-01-02T00-00-00")
                .join(subdir)
                .exists()
        );

        // Verify the newest 5 remain
        for i in 3..=7 {
            assert!(
                base.path()
                    .join(format!("2024-01-0{i}T00-00-00"))
                    .join(subdir)
                    .exists()
            );
        }
    }

    #[test]
    fn restore_component_file_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let backup = dir.path().join("backup.colors");
        let original = dir.path().join("original.colors");
        std::fs::write(&backup, b"backup content").unwrap();
        std::fs::write(&original, b"current content").unwrap();

        restore_component(&backup, &original).unwrap();

        assert_eq!(
            std::fs::read_to_string(&original).unwrap(),
            "backup content"
        );
        // Backup must still exist (restore only copies, does not move)
        assert!(backup.exists());
    }

    #[test]
    fn restore_component_dir_replaces_partial_install() {
        let dir = tempfile::tempdir().unwrap();
        let backup_dir = dir.path().join("backup_pkg");
        let original_dir = dir.path().join("original_pkg");
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(backup_dir.join("metadata.json"), b"{}").unwrap();
        // Simulate a partial (broken) install at original
        std::fs::create_dir_all(&original_dir).unwrap();
        std::fs::write(original_dir.join("broken.txt"), b"partial").unwrap();

        restore_component(&backup_dir, &original_dir).unwrap();

        assert!(original_dir.join("metadata.json").exists());
        assert!(
            !original_dir.join("broken.txt").exists(),
            "partial install content must be replaced"
        );
        // Backup untouched
        assert!(backup_dir.join("metadata.json").exists());
    }

    #[test]
    fn restore_component_dir_creates_original_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let backup_dir = dir.path().join("backup_pkg");
        let original_dir = dir.path().join("original_pkg");
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(backup_dir.join("meta.json"), b"{}").unwrap();

        restore_component(&backup_dir, &original_dir).unwrap();

        assert!(original_dir.join("meta.json").exists());
    }

    #[test]
    fn cleanup_old_backups_noop_when_under_limit() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();

        // Create 3 fake backup dirs (under the limit of 5)
        for i in 1..=3 {
            let ts_dir = base.path().join(format!("2024-01-0{i}T00-00-00"));
            let type_dir = ts_dir.join(subdir);
            std::fs::create_dir_all(&type_dir).unwrap();
        }

        cleanup_old_backups_in(base.path(), subdir, 5, &[], false);

        // All 3 should still exist
        let count = std::fs::read_dir(base.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(subdir).exists())
            .count();
        assert_eq!(count, 3);
    }

    fn make_backup(base: &Path, name: &str, subdir: &str) -> PathBuf {
        let type_dir = base.join(name).join(subdir);
        fs::create_dir_all(&type_dir).unwrap();
        type_dir
    }

    fn component_at(path: PathBuf) -> InstalledComponent {
        widget_at(path, "org.test.widget")
    }

    fn widget_at(path: PathBuf, directory_name: &str) -> InstalledComponent {
        InstalledComponent {
            name: "Test Widget".to_string(),
            directory_name: directory_name.to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path,
            is_system: false,
            release_date: String::new(),
        }
    }

    fn write_package(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("metadata.json"), b"{}").unwrap();
    }

    /// A backup directory name a day or more after this run's own clock, so
    /// the run's backups sort oldest in the retention window.
    fn timestamp_ahead_of_now(days: i64) -> String {
        (chrono::Local::now() + chrono::Duration::days(days))
            .format(BACKUP_TIMESTAMP_FORMAT)
            .to_string()
    }

    /// Waits out the one-second resolution of a backup name, so two backups of
    /// one component type land in two timestamp directories.
    fn wait_for_next_backup_second() {
        let first = timestamp();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while timestamp() == first {
            assert!(
                std::time::Instant::now() < deadline,
                "the backup clock did not tick"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn a_backup_made_this_run_survives_a_later_backup_of_its_type() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();
        for day in 1..=5 {
            make_backup(base.path(), &timestamp_ahead_of_now(day), subdir);
        }
        let first_src = base.path().join("first");
        write_package(&first_src);
        let second_src = base.path().join("second");
        write_package(&second_src);

        let first = backup_component_in(base.path(), &widget_at(first_src, "org.test.first"))
            .unwrap()
            .unwrap();
        wait_for_next_backup_second();
        let second = backup_component_in(base.path(), &widget_at(second_src, "org.test.second"))
            .unwrap()
            .unwrap();

        assert!(
            first.exists(),
            "a later backup of the same type pruned this run's earlier one"
        );
        assert!(second.exists());
    }

    #[test]
    fn pruning_skips_every_directory_this_run_created() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();
        for day in 1..=6 {
            make_backup(base.path(), &format!("2024-01-0{day}T00-00-00"), subdir);
        }
        // Three of the six belong to this run, all named oldest.
        let created: Vec<BackupRecord> = [
            "2024-01-01T00-00-00",
            "2024-01-02T00-00-00",
            "2024-01-03T00-00-00",
        ]
        .iter()
        .map(|name| BackupRecord {
            timestamp_dir: base.path().join(*name),
            type_subdir: subdir,
            system: false,
        })
        .collect();

        prune_backups_in(base.path(), &created, false, 5);

        for day in 1..=3 {
            assert!(
                base.path()
                    .join(format!("2024-01-0{day}T00-00-00"))
                    .join(subdir)
                    .is_dir(),
                "a backup this run made was pruned"
            );
        }
        assert!(
            !base
                .path()
                .join("2024-01-04T00-00-00")
                .join(subdir)
                .exists(),
            "the oldest backup this run did not make must go"
        );
        assert!(
            base.path()
                .join("2024-01-06T00-00-00")
                .join(subdir)
                .is_dir()
        );
    }

    #[test]
    fn cleanup_keeps_the_backup_the_current_run_made() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();
        // The other five are named by a clock ahead of this run, so a name
        // sort puts the run's own backup oldest and would prune it.
        for day in 1..=5 {
            make_backup(base.path(), &format!("2099-01-0{day}T00-00-00"), subdir);
        }
        let current = base.path().join("2026-11-01T09-00-00");
        let current_type = current.join(subdir);
        fs::create_dir_all(&current_type).unwrap();

        cleanup_old_backups_in(
            base.path(),
            subdir,
            5,
            std::slice::from_ref(&current),
            false,
        );

        assert!(current_type.is_dir(), "the run's own backup was pruned");
        assert!(
            !base
                .path()
                .join("2099-01-01T00-00-00")
                .join(subdir)
                .exists(),
            "the oldest other backup must go instead"
        );
        assert!(
            base.path()
                .join("2099-01-05T00-00-00")
                .join(subdir)
                .is_dir()
        );
    }

    #[test]
    fn cleanup_ignores_a_directory_whose_name_is_not_a_backup() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();
        for day in 1..=5 {
            make_backup(base.path(), &format!("2024-01-0{day}T00-00-00"), subdir);
        }
        make_backup(base.path(), "not-a-date", subdir);

        cleanup_old_backups_in(base.path(), subdir, 5, &[], false);

        for day in 1..=5 {
            assert!(
                base.path()
                    .join(format!("2024-01-0{day}T00-00-00"))
                    .join(subdir)
                    .is_dir(),
                "backup {day} was pruned for a directory that is not a backup"
            );
        }
    }

    #[test]
    fn cleanup_orders_backups_by_their_encoded_time() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();
        make_backup(base.path(), "2099-01-01T00-00-00", subdir);
        let older = base.path().join("2020-01-01T00-00-00");
        make_backup(base.path(), "2020-01-01T00-00-00", subdir);
        // A restored cache dir gets fresh mtimes: the backup whose name says
        // it is oldest must still be the one pruned.
        let touched = std::process::Command::new("touch")
            .args(["-d", "2030-01-01T00:00:00"])
            .arg(&older)
            .status()
            .unwrap();
        assert!(touched.success());

        cleanup_old_backups_in(base.path(), subdir, 1, &[], false);

        assert!(
            base.path()
                .join("2099-01-01T00-00-00")
                .join(subdir)
                .is_dir()
        );
        assert!(!older.join(subdir).exists());
    }

    #[test]
    fn cleanup_orders_equal_instants_by_name() {
        let instant =
            chrono::NaiveDateTime::parse_from_str("2024-01-01T00-00-00", BACKUP_TIMESTAMP_FORMAT)
                .unwrap();
        // The same instant under the two spellings a one-digit hour allows,
        // handed over newest-first so nothing but the tie-break can order them.
        let mut backups = vec![
            (instant, PathBuf::from("2024-01-01T00-00-00")),
            (instant, PathBuf::from("2024-01-01T0-00-00")),
        ];

        oldest_first(&mut backups);

        let names: Vec<&str> = backups
            .iter()
            .map(|(_, path)| path.to_str().unwrap())
            .collect();
        assert_eq!(names, ["2024-01-01T0-00-00", "2024-01-01T00-00-00"]);
    }

    #[test]
    fn a_failed_copy_reports_the_os_error_and_its_path() {
        use std::os::unix::fs::PermissionsExt;

        if crate::installer::privilege::is_root() {
            return; // root reads a 000 directory, so no failure can be injected
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let locked = src.join("locked");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&locked).unwrap();
        fs::write(src.join("kept.txt"), b"kept").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let error = copy_dir_recursive(&src, &dst).unwrap_err();

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            error.message.contains(&locked.display().to_string()),
            "{error:?}"
        );
        assert!(error.message.contains("Permission denied"), "{error:?}");
    }

    #[test]
    fn a_failed_backup_leaves_no_partial_backup() {
        use std::os::unix::fs::PermissionsExt;

        if crate::installer::privilege::is_root() {
            return; // root reads a 000 directory, so no failure can be injected
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("backups");
        let src = dir.path().join("pkg");
        let locked = src.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(src.join("metadata.json"), b"{}").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let component = component_at(src.clone());

        let error = backup_component_in(&base, &component).unwrap_err();

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.message.contains("Permission denied"), "{error:?}");
        let leftovers: Vec<PathBuf> = fs::read_dir(&base)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "a partial backup survived: {leftovers:?}"
        );
    }

    #[test]
    fn a_present_component_with_an_unreadable_parent_is_an_error() {
        use std::os::unix::fs::PermissionsExt;

        if crate::installer::privilege::is_root() {
            return; // root reads a 000 directory, so no failure can be injected
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("backups");
        let parent = dir.path().join("parent");
        let component_path = parent.join("org.test.widget");
        fs::create_dir_all(&component_path).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
        let component = component_at(component_path);

        let error = backup_component_in(&base, &component).unwrap_err();

        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.message.contains("Permission denied"), "{error:?}");
    }

    #[test]
    fn copy_tree_refuses_a_fifo_before_opening_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("kept.txt"), b"kept").unwrap();
        let fifo = src.join("pipe");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();

        // Opening a FIFO waits for a writer, so the copy runs on its own
        // thread: a block then reads as a failed check, not a hung suite.
        let (sender, receiver) = std::sync::mpsc::channel();
        let (src_owned, dst_owned) = (src.clone(), dst.clone());
        std::thread::spawn(move || {
            let outcome = copy_dir_recursive(&src_owned, &dst_owned).map_err(|e| e.message);
            let _ = sender.send(outcome);
        });
        let outcome = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the copy opened the FIFO and blocked");

        let message = outcome.expect_err("a FIFO must be refused");
        assert!(message.contains(&fifo.display().to_string()), "{message}");
        assert!(message.contains("not a regular file"), "{message}");
    }

    #[test]
    fn copy_tree_refuses_a_socket_and_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        let socket = src.join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        let error = copy_dir_recursive(&src, &dst).unwrap_err();

        assert!(
            error.message.contains(&socket.display().to_string()),
            "{error:?}"
        );
        assert!(error.message.contains("not a regular file"), "{error:?}");
    }

    #[test]
    fn copy_tree_copies_a_symlink_as_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, src.join("link")).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        let copied = dst.join("link");
        assert!(
            fs::symlink_metadata(&copied)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was followed and its target copied in"
        );
        assert_eq!(fs::read_link(&copied).unwrap(), outside);
    }

    #[test]
    fn copy_tree_preserves_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        let sub = src.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("file.txt"), b"x").unwrap();
        // Neither the 0o755 nor the 0o700 a fresh directory gets under a
        // common umask.
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o750)).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        let mode = fs::metadata(dst.join("sub")).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o750);
    }

    #[test]
    fn copy_tree_takes_a_symlinked_source_mode_from_its_target() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let dst = dir.path().join("dst");
        let sub = real.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("file.txt"), b"x").unwrap();
        // Neither the link's own 0o777 nor a common umask's directory mode.
        fs::set_permissions(&real, fs::Permissions::from_mode(0o750)).unwrap();
        let linked = dir.path().join("linked_component");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        copy_dir_recursive(&linked, &dst).unwrap();

        let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o750, "the link's own mode was copied as the tree's");
        assert!(dst.join("sub").join("file.txt").is_file());
    }

    #[test]
    fn cleanup_prunes_real_backups_past_the_limit_despite_a_junk_directory() {
        let base = tempfile::tempdir().unwrap();
        let subdir = ComponentType::PlasmaWidget.backup_subdir();
        for day in 1..=6 {
            make_backup(base.path(), &format!("2024-01-0{day}T00-00-00"), subdir);
        }
        let junk = make_backup(base.path(), "not-a-date", subdir);
        // The junk directory looks oldest by mtime, so a loop that prunes in
        // that order spends its one removal on it and leaves six backups.
        let touched = std::process::Command::new("touch")
            .args(["-d", "2000-01-01T00:00:00"])
            .arg(junk.parent().unwrap())
            .status()
            .unwrap();
        assert!(touched.success());

        cleanup_old_backups_in(base.path(), subdir, 5, &[], false);

        let named = std::fs::read_dir(base.path())
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("2024-01-"))
                    && entry.path().join(subdir).is_dir()
            })
            .count();
        assert_eq!(named, 5, "a real backup outlived the limit");
        assert!(
            !base
                .path()
                .join("2024-01-01T00-00-00")
                .join(subdir)
                .exists(),
            "the oldest real backup must go"
        );
    }

    #[test]
    fn a_failed_file_backup_leaves_no_empty_directories() {
        use std::os::unix::fs::PermissionsExt;

        if crate::installer::privilege::is_root() {
            return; // root reads a 000 file, so no failure can be injected
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("backups");
        let file = dir.path().join("org.test.widget.colors");
        fs::write(&file, b"[Colors:Window]").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let component = component_at(file.clone());

        let error = backup_component_in(&base, &component).unwrap_err();

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(error.message.contains("Permission denied"), "{error:?}");
        let leftovers: Vec<PathBuf> = fs::read_dir(&base)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "an empty backup directory survived: {leftovers:?}"
        );
    }

    #[test]
    fn a_failed_removal_of_the_partial_backup_file_is_logged() {
        use std::os::unix::fs::PermissionsExt;

        if crate::installer::privilege::is_root() {
            return; // root unlinks out of a 0o500 directory too
        }
        capture_logs();
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("backups");
        let file = dir.path().join("org.test.widget.colors");
        fs::write(&file, b"[Colors:Window]").unwrap();
        let component = component_at(file);

        // The backup path is timestamped inside the call, so the fixture takes
        // its timestamp from a backup this run just made. A clock tick between
        // the two calls lands the second backup in a fresh directory where the
        // copy succeeds: retry, then fail loudly if no attempt reached the
        // refusal.
        let mut logged = None;
        for _ in 0..5 {
            let Ok(Some(backup)) = backup_component_in(&base, &component) else {
                panic!("the fixture's first backup failed");
            };
            let type_dir = backup.parent().unwrap();
            fs::set_permissions(&backup, fs::Permissions::from_mode(0o400)).unwrap();
            fs::set_permissions(type_dir, fs::Permissions::from_mode(0o500)).unwrap();

            let failed = backup_component_in(&base, &component);

            fs::set_permissions(type_dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&backup, fs::Permissions::from_mode(0o644)).unwrap();
            let lines = captured_logs();
            let named = lines.iter().any(|line| {
                line.starts_with("backup: ")
                    && line.contains("failed to remove partial backup")
                    && line.contains(&backup.display().to_string())
            });
            if failed.is_err() && named {
                logged = Some(lines);
                break;
            }
        }

        assert!(
            logged.is_some(),
            "a failed removal of the partial backup file was dropped: {:?}",
            captured_logs()
        );
    }

    /// A same-second retry lands its copy in a type directory an older run left
    /// root-owned. That refusal names the chown fix, not only the OS error.
    #[test]
    fn a_backup_into_an_unwritable_type_dir_fails_naming_the_chown_fix() {
        use std::os::unix::fs::PermissionsExt;

        if crate::installer::privilege::is_root() {
            return; // root writes into a 0o555 directory
        }
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("backups");
        let file = dir.path().join("org.test.widget.colors");
        fs::write(&file, b"[Colors:Window]\n").unwrap();
        let component = component_at(file);
        let subdir = component.component_type.backup_subdir();

        // The backup path is timestamped inside the call, so the fixture takes
        // the type directory of the second the call lands in. A clock tick
        // between the two makes the call create a writable one and succeed:
        // retry, then fail loudly if no attempt reached the refusal.
        let mut refusal = None;
        for _ in 0..5 {
            let type_dir = base.join(timestamp()).join(subdir);
            fs::create_dir_all(&type_dir).unwrap();
            fs::set_permissions(&type_dir, fs::Permissions::from_mode(0o555)).unwrap();

            let failed = backup_component_in(&base, &component);

            // The failed backup removes the type directory it left empty.
            let _ = fs::set_permissions(&type_dir, fs::Permissions::from_mode(0o755));
            if let Err(failed) = failed {
                refusal = Some(failed);
                break;
            }
        }

        let refusal = refusal.expect("a backup into a directory this user cannot write succeeded");
        assert!(
            refusal.message.contains("sudo chown -R"),
            "the refusal names no fix: {}",
            refusal.message
        );
        let cache_dir = crate::paths::cache_home().join("plasmoid-updater");
        assert!(
            refusal.message.contains(&cache_dir.display().to_string()),
            "the hint names no path under the cache dir: {}",
            refusal.message
        );
    }

    const SYSTEM_BACKUP_CHILD: &str = "PLASMOID_UPDATER_SYSTEM_BACKUP_CHILD";
    const SYSTEM_BACKUP_ROOT: &str = "PLASMOID_UPDATER_SYSTEM_BACKUP_ROOT";

    /// A `--system` backup lands under the injected system backups root, never
    /// under the user's cache, and every write runs through sudo.
    #[test]
    fn a_system_backup_lands_under_the_system_base() {
        if crate::installer::privilege::is_root() {
            return; // root takes the plain filesystem branches
        }

        if std::env::var_os(SYSTEM_BACKUP_CHILD).is_some() {
            let root = std::path::PathBuf::from(std::env::var(SYSTEM_BACKUP_ROOT).unwrap());
            let system_base = root.join("system-backups");
            let component_path = root.join("color-schemes/Fake.colors");
            std::fs::create_dir_all(component_path.parent().unwrap()).unwrap();
            std::fs::write(&component_path, "[Colors:Window]\n").unwrap();
            let component = InstalledComponent {
                name: "Fake".to_string(),
                directory_name: "Fake.colors".to_string(),
                version: "1.0".to_string(),
                component_type: ComponentType::ColorScheme,
                path: component_path,
                is_system: true,
                release_date: String::new(),
            };
            let ledger = BackupLedger::default();

            let backup = backup_component_at(&component, &system_base, &ledger, |_| true)
                .unwrap()
                .unwrap();

            assert!(
                backup.starts_with(&system_base),
                "the system backup landed outside the system base: {}",
                backup.display()
            );
            let cache_backups = crate::paths::cache_home().join("plasmoid-updater/backups");
            assert!(
                !cache_backups.exists(),
                "a system backup reached the user cache: {}",
                cache_backups.display()
            );
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sudo = bin.join("sudo");
        std::fs::write(
            &sudo,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PU_TEST_SUDO_LOG\"\nexec \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sudo_log = root.join("sudo.log");

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "installer::backup::tests::a_system_backup_lands_under_the_system_base",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("TMPDIR", root.join("tmp"))
            .env(SYSTEM_BACKUP_CHILD, "1")
            .env(SYSTEM_BACKUP_ROOT, &root)
            .env("PU_TEST_SUDO_LOG", &sudo_log)
            .output()
            .unwrap();

        assert!(
            output.status.success() && root.join("done").exists(),
            "child failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            root.join("done").exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let log = std::fs::read_to_string(&sudo_log).unwrap();
        assert!(
            log.lines().any(|line| line.starts_with("mkdir")),
            "the system backup was not created through sudo: {log}"
        );
        assert!(
            log.lines().any(|line| line.starts_with("cp")),
            "the system backup was not written through sudo: {log}"
        );
    }

    fn system_scheme(dir: &Path, name: &str) -> InstalledComponent {
        InstalledComponent {
            name: name.to_string(),
            directory_name: format!("{name}.colors"),
            version: "1.0".to_string(),
            component_type: ComponentType::ColorScheme,
            path: dir.join(format!("{name}.colors")),
            is_system: true,
            release_date: String::new(),
        }
    }

    struct SudoChild {
        _dir: tempfile::TempDir,
        root: PathBuf,
        output: std::process::Output,
        log: String,
    }

    /// The fixture root when this process is `test`'s child run.
    fn backup_child_root(test: &str) -> Option<PathBuf> {
        let child = std::env::var("PU_BACKUP_CHILD_TEST").ok()?;
        (child == test).then(|| PathBuf::from(std::env::var("PU_BACKUP_ROOT").unwrap()))
    }

    /// Fails unless `sudo` on `PATH` is the fixture's, never the real one.
    fn assert_fake_sudo(root: &Path) {
        let resolved = std::process::Command::new("sh")
            .args(["-c", "command -v sudo"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&resolved.stdout).trim(),
            root.join("bin/sudo").to_string_lossy(),
            "a real sudo would run against a system path"
        );
    }

    /// Re-runs `test` in a child process with a logging fake `sudo` first on
    /// `PATH` that runs the command it is handed, and a fresh fixture root in
    /// `PU_BACKUP_ROOT`.
    fn run_backup_child(test: &str) -> SudoChild {
        run_backup_child_with(test, "exec \"$@\"\n")
    }

    /// Re-runs `test` with a fake `sudo` that logs and exits 1, never running
    /// the command: the production test names `/var/lib/...`, and a fake sudo
    /// that executes would really write there as the test user.
    fn run_backup_child_refusing(test: &str) -> SudoChild {
        run_backup_child_with(test, "exit 1\n")
    }

    fn run_backup_child_with(test: &str, tail: &str) -> SudoChild {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let sudo = bin.join("sudo");
        std::fs::write(
            &sudo,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PU_TEST_SUDO_LOG\"\n{tail}"),
        )
        .unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log_path = root.join("sudo.log");

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg(format!("installer::backup::tests::{test}"))
            .args(["--exact", "--nocapture", "--test-threads=1"])
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("TMPDIR", root.join("tmp"))
            .env("PU_TEST_SUDO_LOG", &log_path)
            .env("PU_BACKUP_CHILD_TEST", test)
            .env("PU_BACKUP_ROOT", &root)
            .output()
            .unwrap();
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        SudoChild {
            _dir: dir,
            root,
            output,
            log,
        }
    }

    #[test]
    fn a_system_component_backs_up_under_the_root_owned_system_base() {
        let mut system = widget_at(
            PathBuf::from("/usr/share/plasma/plasmoids/org.test.system"),
            "org.test.system",
        );
        system.is_system = true;
        let user = widget_at(
            PathBuf::from("/home/user/.local/share/plasma/plasmoids/org.test.user"),
            "org.test.user",
        );

        assert_eq!(
            backup_base_dir_for(&system, &system_backup_base_dir()),
            PathBuf::from("/var/lib/plasmoid-updater/backups"),
            "a system backup no longer lands under the root-owned base"
        );
        assert_eq!(
            backup_base_dir_for(&user, &system_backup_base_dir()),
            crate::paths::cache_home().join("plasmoid-updater/backups"),
            "a user backup left the user cache"
        );
    }

    /// The production system base is the root-owned
    /// `/var/lib/plasmoid-updater/backups` root: a system backup through the
    /// seam with that base and an injected trust verdict makes the fake sudo
    /// log a `mkdir` for it.
    #[test]
    fn a_system_backup_goes_through_the_root_owned_system_base() {
        const TEST: &str = "a_system_backup_goes_through_the_root_owned_system_base";
        if crate::installer::privilege::is_root() {
            return;
        }

        if let Some(root) = backup_child_root(TEST) {
            assert_fake_sudo(&root);
            let scheme_dir = root.join("color-schemes");
            std::fs::create_dir_all(&scheme_dir).unwrap();
            let component = system_scheme(&scheme_dir, "Fake");
            std::fs::write(&component.path, "[Colors:Window]\n").unwrap();
            let ledger = BackupLedger::default();

            // The trust verdict is injected instead of read from the host. The
            // fake sudo refuses to run, so the production base is only ever
            // logged, never created.
            let _ = backup_component_at(&component, &system_backup_base_dir(), &ledger, |_| true);
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let child = run_backup_child_refusing(TEST);
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        assert!(
            child
                .log
                .lines()
                .any(|line| line.contains("/var/lib/plasmoid-updater/backups")),
            "the production base was not used: {}",
            child.log
        );
    }

    /// Every system widget is a directory: its whole tree lands under the
    /// injected system base, copied through sudo, never under the user cache.
    #[test]
    fn a_system_backup_of_a_directory_component_copies_the_tree_through_sudo() {
        const TEST: &str = "a_system_backup_of_a_directory_component_copies_the_tree_through_sudo";
        if crate::installer::privilege::is_root() {
            return;
        }

        if let Some(root) = backup_child_root(TEST) {
            assert_fake_sudo(&root);
            let component_path = root.join("widgets/FakeWidget");
            std::fs::create_dir_all(&component_path).unwrap();
            std::fs::write(component_path.join("metadata.json"), b"{}").unwrap();
            let component = InstalledComponent {
                name: "Fake Widget".to_string(),
                directory_name: "FakeWidget".to_string(),
                version: "1.0".to_string(),
                component_type: ComponentType::PlasmaWidget,
                path: component_path,
                is_system: true,
                release_date: String::new(),
            };
            let system_base = root.join("system-backups");
            let ledger = BackupLedger::default();

            let backup = backup_component_at(&component, &system_base, &ledger, |_| true)
                .unwrap()
                .unwrap();

            assert!(backup.starts_with(&system_base), "{}", backup.display());
            assert!(
                backup.join("metadata.json").is_file(),
                "the backup is not the component's own tree: {}",
                backup.display()
            );
            let cache_backups = crate::paths::cache_home().join("plasmoid-updater/backups");
            assert!(
                !cache_backups.exists(),
                "a system backup reached the user cache: {}",
                cache_backups.display()
            );
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let child = run_backup_child(TEST);
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        assert!(
            child.log.lines().any(|line| line.starts_with("mkdir")),
            "the directory backup was not created through sudo: {}",
            child.log
        );
        assert!(
            child.log.lines().any(|line| line.starts_with("cp -rfT")),
            "the directory backup was not copied through sudo: {}",
            child.log
        );
    }

    /// A system copy that fails removes its half-written tree through sudo and
    /// leaves no empty backup directory behind.
    #[test]
    fn a_failed_system_copy_removes_its_directories_through_sudo() {
        const TEST: &str = "a_failed_system_copy_removes_its_directories_through_sudo";
        if crate::installer::privilege::is_root() {
            return;
        }

        if let Some(root) = backup_child_root(TEST) {
            assert_fake_sudo(&root);
            let scheme_dir = root.join("color-schemes");
            std::fs::create_dir_all(&scheme_dir).unwrap();
            let component = system_scheme(&scheme_dir, "Fake");
            std::fs::write(&component.path, "[Colors:Window]\n").unwrap();
            std::fs::set_permissions(&component.path, std::fs::Permissions::from_mode(0o000))
                .unwrap();
            let system_base = root.join("system-backups");
            let ledger = BackupLedger::default();

            let failed = backup_component_at(&component, &system_base, &ledger, |_| true);
            let _ =
                std::fs::set_permissions(&component.path, std::fs::Permissions::from_mode(0o644));
            assert!(
                failed.is_err(),
                "a copy that could not read its source succeeded"
            );

            let leftovers: Vec<PathBuf> = std::fs::read_dir(&system_base)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .collect();
            assert!(
                leftovers.is_empty(),
                "an empty backup directory survived: {leftovers:?}"
            );
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let child = run_backup_child(TEST);
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        assert!(
            child.log.lines().any(|line| line.starts_with("rm -f")),
            "the partial backup was not removed through sudo: {}",
            child.log
        );
        assert!(
            child.log.lines().any(|line| line.starts_with("rmdir")),
            "the empty backup directories were not removed through sudo: {}",
            child.log
        );
    }

    /// Retention on a system base lists and removes through sudo, so it runs
    /// even when the root-owned base is not listable by this user.
    #[test]
    fn a_system_prune_lists_and_removes_through_sudo() {
        const TEST: &str = "a_system_prune_lists_and_removes_through_sudo";
        if crate::installer::privilege::is_root() {
            return;
        }

        if let Some(root) = backup_child_root(TEST) {
            assert_fake_sudo(&root);
            let base = root.join("system-backups");
            let subdir = ComponentType::PlasmaWidget.backup_subdir();
            for day in 1..=6 {
                std::fs::create_dir_all(base.join(format!("2024-01-0{day}T00-00-00")).join(subdir))
                    .unwrap();
            }
            let ledger = BackupLedger::default();
            // The newest is this run's own and must survive.
            ledger.record(&base.join("2024-01-06T00-00-00"), subdir, true);

            prune_backups(&ledger);

            assert!(
                !base.join("2024-01-01T00-00-00").join(subdir).exists(),
                "the oldest system backup must go"
            );
            for day in 2..=6 {
                assert!(
                    base.join(format!("2024-01-0{day}T00-00-00"))
                        .join(subdir)
                        .is_dir(),
                    "backup {day} was pruned"
                );
            }
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let child = run_backup_child(TEST);
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        assert!(
            child.log.lines().any(|line| line.starts_with("find ")),
            "the system backups were not listed through sudo: {}",
            child.log
        );
        assert!(
            child.log.lines().any(|line| line.starts_with("rm -rf")),
            "the pruned system backup was not removed through sudo: {}",
            child.log
        );
    }

    /// The system base is created with its own mode, and a base the trust
    /// check refuses is never written into.
    #[test]
    fn a_system_backup_base_is_created_root_private_and_refused_when_untrusted() {
        const TEST: &str =
            "a_system_backup_base_is_created_root_private_and_refused_when_untrusted";
        if crate::installer::privilege::is_root() {
            return;
        }

        if let Some(root) = backup_child_root(TEST) {
            assert_fake_sudo(&root);
            let scheme_dir = root.join("color-schemes");
            std::fs::create_dir_all(&scheme_dir).unwrap();
            let component = system_scheme(&scheme_dir, "Fake");
            std::fs::write(&component.path, "[Colors:Window]\n").unwrap();
            let system_base = root.join("system-backups");
            let ledger = BackupLedger::default();

            let refusal = backup_component_at(&component, &system_base, &ledger, |path| {
                path != system_base
            })
            .expect_err("a base the trust check refuses received a backup");
            assert_eq!(
                refusal.message,
                format!(
                    "refusing to write a system backup into {}: it, or a directory above it, is a symlink, not owned by root, or writable by group or others",
                    system_base.display()
                ),
            );
            let written: Vec<PathBuf> = std::fs::read_dir(&system_base)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .collect();
            assert!(
                written.is_empty(),
                "a refused base was written into: {written:?}"
            );
            let mode = std::fs::metadata(&system_base)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777;
            assert_eq!(mode, 0o755, "the base took the process umask for its mode");

            let backup = backup_component_at(&component, &system_base, &ledger, |_| true)
                .unwrap()
                .unwrap();
            assert!(backup.starts_with(&system_base), "{}", backup.display());
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let child = run_backup_child(TEST);
        assert!(
            child.output.status.success() && child.root.join("done").exists(),
            "child failed ({})\n--- stderr\n{}",
            child.output.status,
            String::from_utf8_lossy(&child.output.stderr),
        );
        assert!(
            child
                .log
                .lines()
                .any(|line| line.starts_with("mkdir") && line.contains("-m 755")),
            "the base was not created with its own mode: {}",
            child.log
        );
    }

    /// The pasted command shell-quotes its path, and every clause of the hint
    /// (the refusing directory, the OS error, the cache dir, the fix) is pinned
    /// by equality.
    #[test]
    fn the_chown_hint_shell_quotes_a_path_with_a_space_and_a_quote() {
        let dir = Path::new("/cache/plasmoid-updater/backups/2024-01-01T00-00-00/color-schemes");
        let cache_dir = Path::new("/home/a b'c/.cache/plasmoid-updater");

        let message = chown_hint(dir, Some(&std::io::Error::from_raw_os_error(13)), cache_dir);

        assert_eq!(
            message,
            format!(
                "{}: Permission denied (os error 13); {} is not writable by this user: sudo chown -R \"$USER\": '/home/a b'\\''c/.cache/plasmoid-updater'",
                dir.display(),
                cache_dir.display(),
            )
        );
    }

    /// A permission refusal copying a directory names the chown fix and keeps
    /// the operating system's error, the way the single-file copy does.
    #[test]
    fn a_permission_failure_copying_a_directory_names_the_chown_fix_and_keeps_the_os_error() {
        use std::os::unix::fs::PermissionsExt as _;

        if crate::installer::privilege::is_root() {
            return; // root writes into a 0o555 directory
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("file.txt"), b"x").unwrap();
        let dst = dir.path().join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o555)).unwrap();

        let failure = copy_backup_dir(&src, &dst, false).unwrap_err();

        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            failure.message.contains("sudo chown -R"),
            "the chown fix is missing: {}",
            failure.message
        );
        assert!(
            failure.message.contains("Permission denied"),
            "the OS error was dropped: {}",
            failure.message
        );
    }

    /// A non-permission failure copying a directory keeps its own message:
    /// the hint keys on the error's kind, never on the destination's mode
    /// bits, so a disk-full or wrong-shape source is never read as "not
    /// writable by this user".
    #[test]
    fn a_non_permission_failure_copying_a_directory_keeps_its_own_message() {
        use std::os::unix::fs::PermissionsExt as _;

        if crate::installer::privilege::is_root() {
            return; // root writes into a 0o555 directory
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::write(&src, b"not a directory").unwrap();
        let dst = dir.path().join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o555)).unwrap();

        let failure = copy_backup_dir(&src, &dst, false).unwrap_err();

        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !failure.message.contains("sudo chown -R"),
            "the hint fired on a non-permission failure: {}",
            failure.message
        );
        assert!(
            failure.message.contains("source is not a directory"),
            "the failure's own message was replaced: {}",
            failure.message
        );
    }

    /// A file backup under a base the invoking user cannot search (a root-owned
    /// 0700 one, stood in for by a mode with no execute bit) is refused at
    /// backup time, naming the level and its fix, before the restore can
    /// misroute the file to the directory branch and fail with `RestoreFailed`.
    #[test]
    fn a_file_backup_under_an_unsearchable_base_is_refused_before_the_restore_misroutes() {
        use std::os::unix::fs::PermissionsExt as _;

        if crate::installer::privilege::is_root() {
            return; // root searches a directory whatever its mode
        }
        let dir = tempfile::tempdir().unwrap();
        let system_base = dir.path().join("backups");
        std::fs::create_dir_all(&system_base).unwrap();
        std::fs::set_permissions(&system_base, std::fs::Permissions::from_mode(0o000)).unwrap();

        let scheme_dir = dir.path().join("color-schemes");
        std::fs::create_dir_all(&scheme_dir).unwrap();
        let component = system_scheme(&scheme_dir, "Fake");
        std::fs::write(&component.path, "[Colors:Window]\n").unwrap();
        let ledger = BackupLedger::default();

        let refusal = backup_component_at(&component, &system_base, &ledger, |_| true).unwrap_err();

        std::fs::set_permissions(&system_base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            refusal.kind,
            FailureKind::Install,
            "the refusal must not read as a failed restore"
        );
        assert_eq!(
            refusal.message,
            format!(
                "refusing to write a system backup into {}: {} is not searchable by this user: sudo chmod 755 '{}'",
                system_base.display(),
                system_base.display(),
                system_base.display(),
            ),
        );
        assert!(
            std::fs::read_dir(&system_base).unwrap().next().is_none(),
            "a refused base was written into"
        );
    }

    /// The refusal for an unsearchable level names the level itself (an
    /// existing base or an existing parent) and its fix, both shell-quoted.
    #[test]
    fn an_unsearchable_system_backup_base_or_parent_is_refused_naming_the_level() {
        use std::os::unix::fs::PermissionsExt as _;

        if crate::installer::privilege::is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        // An existing base without the other-execute bit.
        let base = dir.path().join("backups");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refusal = ensure_system_backup_base(&base, |_| true).unwrap_err();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(refusal.kind, FailureKind::Install);
        assert_eq!(
            refusal.message,
            format!(
                "refusing to write a system backup into {}: {} is not searchable by this user: sudo chmod 755 '{}'",
                base.display(),
                base.display(),
                base.display(),
            ),
        );

        // An existing parent without the other-execute bit.
        let parent = dir.path().join("parent");
        let base = parent.join("backups");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refusal = ensure_system_backup_base(&base, |_| true).unwrap_err();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(refusal.kind, FailureKind::Install);
        assert_eq!(
            refusal.message,
            format!(
                "refusing to write a system backup into {}: {} is not searchable by this user: sudo chmod 755 '{}'",
                base.display(),
                parent.display(),
                parent.display(),
            ),
        );
    }

    /// A missing base under a symlinked ancestor is refused before root's
    /// `mkdir` can follow the link, so the backup never lands where it points.
    #[test]
    fn a_missing_base_under_a_symlinked_ancestor_is_refused_before_root_follows_it() {
        if crate::installer::privilege::is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        let base = dir.path().join("link").join("backups");

        let refusal = ensure_system_backup_base(&base, |path| !path.is_symlink()).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Install);
        assert_eq!(
            refusal.message,
            format!(
                "refusing to write a system backup into {}: it, or a directory above it, is a symlink, not owned by root, or writable by group or others",
                base.display()
            ),
        );
        assert!(
            !target.join("backups").exists(),
            "root's mkdir followed the symlinked ancestor"
        );
    }

    /// A non-UTF-8 cache path cannot be shell-quoted losslessly, so the chown
    /// hint names it with `{:?}` and drops the pasteable command.
    #[test]
    fn a_non_utf8_cache_path_hint_names_the_path_without_a_pasteable_command() {
        use std::os::unix::ffi::OsStringExt as _;

        let dir = Path::new("/cache/plasmoid-updater/backups/2024-01-01T00-00-00/color-schemes");
        let mut name = b"/home/".to_vec();
        name.push(0xFF);
        name.extend_from_slice(b"/.cache/plasmoid-updater");
        let cache_dir = PathBuf::from(std::ffi::OsString::from_vec(name));

        let source = std::io::Error::from_raw_os_error(13);
        let message = chown_hint(dir, Some(&source), &cache_dir);

        assert!(
            !message.contains("sudo chown"),
            "a non-UTF-8 path must not get a lossy pasteable command: {message}"
        );
        assert!(
            message.contains(&format!("{cache_dir:?}")),
            "the non-UTF-8 path must be named losslessly: {message}"
        );
        assert_eq!(
            message,
            format!(
                "{}: {source}; {cache_dir:?} is not writable by this user",
                dir.display(),
            ),
            "the non-UTF-8 hint must name every clause"
        );
    }

    /// The `find` listing passes the base as an `OsStr`, so a non-UTF-8 base
    /// reaches sudo byte-for-byte instead of as U+FFFD.
    #[test]
    fn a_system_backup_listing_passes_a_non_utf8_base_to_sudo() {
        use std::os::unix::ffi::OsStringExt as _;
        use std::os::unix::fs::PermissionsExt as _;

        if crate::installer::privilege::is_root() {
            return; // root lists directly, without sudo
        }

        if std::env::var_os("PU_TEST_FIND_CHILD").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_FIND_ROOT").unwrap());
            let fake = std::env::var("PU_TEST_FAKE_SUDO").unwrap();
            let resolved = std::process::Command::new("sh")
                .args(["-c", "command -v sudo"])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&resolved.stdout).trim(),
                fake,
                "a real sudo would run against a system path"
            );

            let mut name = b"base-".to_vec();
            name.push(0xFF);
            let base = root.join(std::ffi::OsString::from_vec(name));
            std::fs::create_dir_all(&base).unwrap();

            let dirs = system_backup_dirs(&base, "plasma-plasmoids");

            assert!(dirs.is_empty(), "{dirs:?}");
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_sudo = bin.join("sudo");
        std::fs::write(
            &fake_sudo,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PU_TEST_SUDO_LOG\"\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();

        let log = root.join("sudo.log");
        let path = std::ffi::OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::backup::tests::a_system_backup_listing_passes_a_non_utf8_base_to_sudo",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_SUDO_LOG", &log)
            .env("PU_TEST_FIND_CHILD", "1")
            .env("PU_TEST_FIND_ROOT", &root)
            .env("PU_TEST_FAKE_SUDO", &fake_sudo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let log = std::fs::read(&log).unwrap();
        assert!(
            log.contains(&0xFF),
            "the find base lost the non-UTF-8 byte: {:?}",
            String::from_utf8_lossy(&log)
        );
    }
}
