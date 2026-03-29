// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    types::InstalledComponent,
    {Error, Result},
};

const MAX_BACKUPS_PER_TYPE: usize = 5;

/// Returns the base backup directory.
fn backup_base_dir() -> PathBuf {
    crate::paths::cache_home().join("plasmoid-updater/backups")
}

/// Generates a timestamp string for backup directories.
fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string()
}

/// Creates a backup of the component before updating.
/// Returns the path to the backup directory or file.
pub(crate) fn backup_component(component: &InstalledComponent) -> Result<PathBuf> {
    let timestamp = timestamp();
    let base = backup_base_dir();
    let type_dir = component.component_type.backup_subdir();

    // handle single files (e.g., color schemes, static wallpapers)
    if component.path.is_file() {
        let backup_dir = base.join(&timestamp).join(type_dir);
        fs::create_dir_all(&backup_dir).map_err(|e| Error::backup(format!("create dir: {e}")))?;

        let backup_path = backup_dir.join(&component.directory_name);
        fs::copy(&component.path, &backup_path)
            .map_err(|e| Error::backup(format!("copy file: {e}")))?;

        // Prune old backups for this component type
        cleanup_old_backups(component.component_type);

        return Ok(backup_path);
    }

    let backup_path = base
        .join(&timestamp)
        .join(type_dir)
        .join(&component.directory_name);

    fs::create_dir_all(&backup_path).map_err(|e| Error::backup(format!("create dir: {e}")))?;

    copy_dir_recursive(&component.path, &backup_path)?;

    // Prune old backups for this component type
    cleanup_old_backups(component.component_type);

    Ok(backup_path)
}

/// Restores a component from backup.
pub(crate) fn restore_component(backup_path: &Path, original_path: &Path) -> Result<()> {
    use super::privilege;

    // handle single files
    if backup_path.is_file() {
        if let Some(parent) = original_path.parent() {
            privilege::create_dir_all(parent)
                .map_err(|e| Error::backup(format!("create parent dir: {e}")))?;
        }

        privilege::copy_file(backup_path, original_path)
            .map_err(|e| Error::backup(format!("restore file: {e}")))?;

        return Ok(());
    }

    if original_path.exists() {
        privilege::remove_dir_all(original_path)
            .map_err(|e| Error::backup(format!("remove failed install: {e}")))?;
    }

    if let Some(parent) = original_path.parent() {
        privilege::create_dir_all(parent)
            .map_err(|e| Error::backup(format!("create parent dir: {e}")))?;
    }

    privilege::copy_dir(backup_path, original_path)
        .map_err(|e| Error::backup(format!("copy dir: {e}")))?;

    Ok(())
}

pub(super) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Err(Error::backup(format!(
            "source is not a directory: {}",
            src.display()
        )));
    }

    fs::create_dir_all(dst).map_err(|e| Error::backup(format!("create dir: {e}")))?;

    let options = fs_extra::dir::CopyOptions::new()
        .content_only(true)
        .overwrite(true);

    fs_extra::dir::copy(src, dst, &options).map_err(|e| Error::backup(format!("copy dir: {e}")))?;

    Ok(())
}

/// Removes old backup directories, keeping only the most recent `max_keep`.
fn cleanup_old_backups_in(base: &Path, type_subdir: &str, max_keep: usize) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };

    // Collect timestamp dirs that contain this component type's backup
    let mut dirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if !path.is_dir() {
                return None;
            }
            let type_path = path.join(type_subdir);
            if !type_path.exists() {
                return None;
            }
            let name = e.file_name().to_string_lossy().to_string();
            Some((name, path))
        })
        .collect();

    if dirs.len() <= max_keep {
        return;
    }

    // Sort by timestamp name (lexicographic = chronological for ISO format)
    dirs.sort_by(|a, b| a.0.cmp(&b.0));

    // Remove oldest, keep last max_keep
    let to_remove = dirs.len() - max_keep;
    for (_, path) in dirs.into_iter().take(to_remove) {
        let type_path = path.join(type_subdir);
        if let Err(e) = fs::remove_dir_all(&type_path) {
            log::debug!(target: "backup", "failed to remove old backup {}: {e}", type_path.display());
        }
        // Remove the timestamp dir too if it's now empty
        if path.read_dir().map_or(true, |mut d| d.next().is_none()) {
            let _ = fs::remove_dir(&path);
        }
    }
}

/// Removes old backups for a component type, keeping the most recent ones.
pub(crate) fn cleanup_old_backups(component_type: crate::types::ComponentType) {
    cleanup_old_backups_in(
        &backup_base_dir(),
        component_type.backup_subdir(),
        MAX_BACKUPS_PER_TYPE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
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
        cleanup_old_backups_in(base.path(), subdir, 5);

        // Verify only 5 remain
        let count_after = std::fs::read_dir(base.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(subdir).exists())
            .count();
        assert_eq!(count_after, 5);

        // Verify the oldest 2 were removed (01 and 02)
        assert!(!base
            .path()
            .join("2024-01-01T00-00-00")
            .join(subdir)
            .exists());
        assert!(!base
            .path()
            .join("2024-01-02T00-00-00")
            .join(subdir)
            .exists());

        // Verify the newest 5 remain
        for i in 3..=7 {
            assert!(base
                .path()
                .join(format!("2024-01-0{i}T00-00-00"))
                .join(subdir)
                .exists());
        }
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

        cleanup_old_backups_in(base.path(), subdir, 5);

        // All 3 should still exist
        let count = std::fs::read_dir(base.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(subdir).exists())
            .count();
        assert_eq!(count, 3);
    }
}
