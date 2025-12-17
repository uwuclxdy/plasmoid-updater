// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{Error, InstalledComponent, Result};

/// returns the base backup directory.
fn backup_base_dir() -> PathBuf {
    crate::paths::cache_home().join("plasmoid-updater/backups")
}

/// returns a subdirectory name based on component type for organization.
pub(crate) fn type_subdir(component: &InstalledComponent) -> &'static str {
    component.component_type.backup_subdir()
}

/// generates a timestamp string for backup directories.
fn timestamp() -> String {
    use std::time::SystemTime;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // format as YYYY-MM-DDTHH-MM-SS
    let secs_per_min = 60;
    let secs_per_hour = 3600;
    let secs_per_day = 86400;

    let days = now / secs_per_day;
    let remaining = now % secs_per_day;
    let hours = remaining / secs_per_hour;
    let remaining = remaining % secs_per_hour;
    let minutes = remaining / secs_per_min;
    let seconds = remaining % secs_per_min;

    // simple year/month/day calculation (approximate, ignores leap years for simplicity)
    let years = 1970 + days / 365;
    let day_of_year = days % 365;

    let months = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    let mut day = day_of_year;

    for (i, &days_in_month) in months.iter().enumerate() {
        if day < days_in_month {
            month = i + 1;
            break;
        }
        day -= days_in_month;
    }

    if month == 0 {
        month = 12;
    }

    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}",
        years,
        month,
        day + 1,
        hours,
        minutes,
        seconds
    )
}

/// creates a backup of the component before updating.
/// returns the path to the backup directory or file.
pub fn backup_component(component: &InstalledComponent) -> Result<PathBuf> {
    let timestamp = timestamp();
    let base = backup_base_dir();
    let type_dir = type_subdir(component);

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

/// restores a component from backup.
pub fn restore_from_backup(backup_path: &Path, original_path: &Path) -> Result<()> {
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

    fs::create_dir_all(dst).map_err(|e| Error::backup(format!("create dst dir: {e}")))?;

    for entry in fs::read_dir(src).map_err(|e| Error::backup(format!("read dir: {e}")))? {
        let entry = entry.map_err(|e| Error::backup(format!("read entry: {e}")))?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);

        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        if metadata.is_dir() {
            copy_dir_recursive(&path, &dst_path)?;
        } else if metadata.is_symlink() {
            // copy symlinks as-is
            if let Ok(target) = fs::read_link(&path) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink;
                    let _ = symlink(&target, &dst_path);
                }
            }
        } else {
            fs::copy(&path, &dst_path).map_err(|e| Error::backup(format!("copy file: {e}")))?;
        }
    }

    Ok(())
}
