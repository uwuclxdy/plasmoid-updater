// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{Error, InstalledComponent, Result};

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
pub fn backup_component(component: &InstalledComponent) -> Result<PathBuf> {
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

        return Ok(backup_path);
    }

    let backup_path = base
        .join(&timestamp)
        .join(type_dir)
        .join(&component.directory_name);

    fs::create_dir_all(&backup_path).map_err(|e| Error::backup(format!("create dir: {e}")))?;

    copy_dir_recursive(&component.path, &backup_path)?;

    Ok(backup_path)
}

/// Restores a component from backup.
pub fn restore_component(backup_path: &Path, original_path: &Path) -> Result<()> {
    // handle single files
    if backup_path.is_file() {
        if let Some(parent) = original_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::backup(format!("create parent dir: {e}")))?;
        }

        fs::copy(backup_path, original_path)
            .map_err(|e| Error::backup(format!("restore file: {e}")))?;

        return Ok(());
    }

    if original_path.exists() {
        fs::remove_dir_all(original_path)
            .map_err(|e| Error::backup(format!("remove failed install: {e}")))?;
    }

    if let Some(parent) = original_path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::backup(format!("create parent dir: {e}")))?;
    }

    copy_dir_recursive(backup_path, original_path)?;

    Ok(())
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
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
