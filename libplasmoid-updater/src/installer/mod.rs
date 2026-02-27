// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

mod backup;
mod download;
mod install;
mod plasmashell;
pub(crate) mod privilege;

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    registry,
    types::{AvailableUpdate, InstalledComponent},
    {Error, Result},
};
use backup::{backup_component, restore_component};

pub(crate) use plasmashell::{any_requires_restart, restart_plasmashell};

/// Updates a single component using provided HTTP client.
pub(crate) fn update_component(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
) -> Result<()> {
    let component = &update.installed;

    let backup_path = create_backup(component)?;

    match perform_installation(update, client) {
        Ok(()) => {
            post_install_tasks(update)?;
            log::info!(target: "update", "updated {}", component.name);
            Ok(())
        }
        Err(e) => {
            log::error!(target: "install", "failed for {}: {e}", component.name);
            handle_installation_failure(&backup_path, &component.path)?;
            Err(e)
        }
    }
}

fn create_backup(component: &InstalledComponent) -> Result<PathBuf> {
    let backup_path = backup_component(component)?;
    log::debug!(target: "backup", "created at {}", backup_path.display());
    Ok(backup_path)
}

fn perform_installation(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
) -> Result<()> {
    let component = &update.installed;
    let downloaded_path = download_with_error_handling(
        client,
        &update.download_url,
        update.checksum.as_deref(),
        &component.name,
    )?;

    execute_installation(&downloaded_path, component, &update.latest_version)
}

fn download_with_error_handling(
    client: &reqwest::blocking::Client,
    url: &str,
    checksum: Option<&str>,
    component_name: &str,
) -> Result<PathBuf> {
    download::download_package(client, url, checksum).map_err(|e| {
        log::error!(target: "download", "failed for {}: {e}", component_name);
        e
    })
}

fn execute_installation(
    downloaded_path: &Path,
    component: &InstalledComponent,
    new_version: &str,
) -> Result<()> {
    if install::is_single_file_component(downloaded_path, component.component_type) {
        let result = install::install_raw_file(downloaded_path, component);
        let _ = fs::remove_file(downloaded_path);
        result
    } else {
        install_from_archive(downloaded_path, component, new_version)
    }
}

fn install_from_archive(
    downloaded_path: &Path,
    component: &InstalledComponent,
    new_version: &str,
) -> Result<()> {
    let extract_dir = download::temp_dir().join(format!("extract-{}", component.directory_name));

    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)?;
    }

    if let Err(e) = download::extract_archive(downloaded_path, &extract_dir) {
        log::error!(target: "extract", "failed for {}: {e}", component.name);
        let _ = fs::remove_file(downloaded_path);
        return Err(e);
    }

    let _ = fs::remove_file(downloaded_path);

    let result = if component.component_type.kpackage_type().is_some() {
        install::install_via_kpackage(&extract_dir, component, new_version)
    } else {
        install::install_direct(&extract_dir, component)
    };

    let _ = fs::remove_dir_all(&extract_dir);
    result
}

fn post_install_tasks(update: &AvailableUpdate) -> Result<()> {
    let component = &update.installed;

    let installed_json = component.path.join("metadata.json");
    let installed_desktop = component.path.join("metadata.desktop");

    if installed_json.exists() {
        if let Err(e) = install::patch_metadata(
            &installed_json,
            component.component_type,
            &update.latest_version,
        ) {
            log::warn!(target: "patch", "failed to update installed metadata: {e}");
        }
    } else if installed_desktop.exists()
        && let Err(e) = install::patch_metadata_desktop(&installed_desktop, &update.latest_version)
    {
        log::warn!(target: "patch", "failed to update installed metadata.desktop: {e}");
    }

    if let Err(e) = registry::update_registry_after_install(update) {
        log::warn!(target: "registry", "failed to update: {e}");
    }

    Ok(())
}

fn handle_installation_failure(backup_path: &Path, component_path: &Path) -> Result<()> {
    if let Err(restore_err) = restore_component(backup_path, component_path) {
        log::error!(target: "restore", "failed: {restore_err}");
        Err(Error::other(format!(
            "installation failed and restore failed: {restore_err}"
        )))
    } else {
        log::info!(target: "restore", "no changes were made");
        Ok(())
    }
}
