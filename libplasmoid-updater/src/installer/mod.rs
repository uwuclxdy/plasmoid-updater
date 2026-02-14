// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

mod download;
mod install;
mod plasmashell;

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{
    AvailableUpdate, Error, InstalledComponent, Result, UpdateSummary, backup_component, registry,
    restore_component,
};

pub use download::{download_package, extract_archive};
pub use install::{
    find_metadata_json, install_direct, install_via_kpackagetool, is_single_file_component,
    patch_metadata,
};
pub use plasmashell::{any_requires_restart, requires_plasmashell_restart, restart_plasmashell};

/// Updates a single component using provided HTTP client.
pub fn update_component(
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

/// Updates multiple components sequentially with a provided HTTP client.
///
/// Components in the `excluded` list are skipped and recorded in the summary.
pub fn update_components(
    updates: &[AvailableUpdate],
    excluded: &[String],
    client: &reqwest::blocking::Client,
) -> UpdateSummary {
    let mut summary = UpdateSummary::default();

    for update in updates {
        let name = update.installed.name.clone();
        let dir_name = &update.installed.directory_name;

        if excluded.iter().any(|e| e == dir_name || e == &name) {
            log::debug!(target: "update", "skipping {} (excluded)", name);
            summary.add_skipped(name);
            continue;
        }

        match update_component(update, client) {
            Ok(()) => summary.add_success(name),
            Err(e) => summary.add_failure(name, e.to_string()),
        }
    }

    summary
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

    execute_installation(
        &downloaded_path,
        component,
        &update.latest_version,
    )
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

    let installed_metadata = component.path.join("metadata.json");
    if installed_metadata.exists()
        && let Err(e) = install::patch_metadata(
            &installed_metadata,
            component.component_type,
            &update.latest_version,
        )
    {
        log::warn!(target: "patch", "failed to update installed metadata: {e}");
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
