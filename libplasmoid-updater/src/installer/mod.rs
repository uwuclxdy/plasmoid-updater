// SPDX-License-Identifier: GPL-3.0-or-later
//
// Installation logic based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

mod backup;
mod download;
mod inhibit;
mod install;
mod lock;
mod plasmashell;
pub(crate) mod privilege;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicUsize,
};

use crate::{
    registry,
    types::{AvailableUpdate, InstalledComponent},
    {Error, Result},
};
use backup::{backup_component, restore_component};

use crate::version::normalize_version;

pub(crate) use inhibit::InhibitGuard;
pub(crate) use lock::UpdateLock;
pub(crate) use plasmashell::{any_requires_restart, restart_plasmashell};

/// Outcome of a single component update, including post-install verification.
#[allow(dead_code)]
pub(crate) struct InstallOutcome {
    /// `true` if the post-install version matches the expected version.
    pub verified: bool,
    /// The version we expected to install.
    pub expected_version: String,
    /// The version actually found on disk after install, if readable.
    pub actual_version: Option<String>,
}

/// Updates a single component using the provided HTTP client.
///
/// `reporter` is called with a stage number as each phase completes:
/// - `1` — backup done, download starting
/// - `2` — download done, extraction starting
/// - `3` — extraction done, install starting
///
/// `counter` is incremented once for each HTTP request made.
pub(crate) fn update_component(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
    reporter: impl Fn(u8),
    counter: &AtomicUsize,
) -> Result<InstallOutcome> {
    let component = &update.installed;

    let backup_path = create_backup(component)?;
    reporter(1);

    match perform_installation(update, client, &reporter, counter) {
        Ok(()) => {
            post_install_tasks(update)?;
            let outcome = verify_installed_version(update);
            log::info!(target: "update", "updated {}", component.name);
            Ok(outcome)
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
    reporter: &dyn Fn(u8),
    counter: &AtomicUsize,
) -> Result<()> {
    let component = &update.installed;
    let downloaded_path = download_with_error_handling(
        client,
        &update.download_url,
        update.checksum.as_deref(),
        &component.name,
        &component.directory_name,
        counter,
    )?;
    reporter(2);

    execute_installation(
        &downloaded_path,
        component,
        &update.latest_version,
        reporter,
    )
}

fn download_with_error_handling(
    client: &reqwest::blocking::Client,
    url: &str,
    checksum: Option<&str>,
    component_name: &str,
    directory_name: &str,
    counter: &AtomicUsize,
) -> Result<PathBuf> {
    download::download_package(client, url, checksum, directory_name, counter).map_err(|e| {
        log::error!(target: "download", "failed for {}: {e}", component_name);
        e
    })
}

fn execute_installation(
    downloaded_path: &Path,
    component: &InstalledComponent,
    new_version: &str,
    reporter: &dyn Fn(u8),
) -> Result<()> {
    if install::is_single_file_component(downloaded_path, component.component_type) {
        let result = install::install_raw_file(downloaded_path, component);
        let _ = fs::remove_file(downloaded_path);
        reporter(3);
        result
    } else {
        install_from_archive(downloaded_path, component, new_version, reporter)
    }
}

fn install_from_archive(
    downloaded_path: &Path,
    component: &InstalledComponent,
    new_version: &str,
    reporter: &dyn Fn(u8),
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
    reporter(3);

    let result = if component.component_type.kpackage_type().is_some() {
        match install::install_via_kpackage(&extract_dir, component, new_version) {
            Ok(()) => Ok(()),
            Err(e) if component.component_type.has_direct_fallback() => {
                log::warn!(
                    target: "install",
                    "kpackagetool6 failed for {}, falling back to direct install: {e}",
                    component.name,
                );
                install::install_direct(&extract_dir, component)
            }
            Err(e) => Err(e),
        }
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

fn verify_installed_version(update: &AvailableUpdate) -> InstallOutcome {
    let component = &update.installed;
    let expected = &update.latest_version;

    let actual = read_installed_version(component);

    let verified = match &actual {
        Some(v) => normalize_version(v) == normalize_version(expected),
        None => false,
    };

    if verified {
        log::debug!(
            target: "verify",
            "{}: version {} confirmed",
            component.name, expected,
        );
    } else {
        log::warn!(
            target: "verify",
            "{}: expected version {}, found {}",
            component.name,
            expected,
            actual.as_deref().unwrap_or("(unreadable)"),
        );
    }

    InstallOutcome {
        verified,
        expected_version: expected.clone(),
        actual_version: actual,
    }
}

fn read_installed_version(component: &InstalledComponent) -> Option<String> {
    // For registry-only types, read from the KNewStuff registry
    if component.component_type.registry_only() {
        return read_version_from_registry(component);
    }

    // Try metadata.json first
    let json_path = component.path.join("metadata.json");
    if json_path.exists()
        && let Ok(content) = fs::read_to_string(&json_path)
        && let Ok(meta) = serde_json::from_str::<crate::types::PackageMetadata>(&content)
        && let Some(v) = meta.version()
    {
        return Some(v.to_string());
    }

    // Fall back to metadata.desktop
    let desktop_path = component.path.join("metadata.desktop");
    if desktop_path.exists()
        && let Ok(content) = fs::read_to_string(&desktop_path)
    {
        for line in content.lines() {
            if let Some(version) = line.strip_prefix("X-KDE-PluginInfo-Version=") {
                return Some(version.to_string());
            }
        }
    }

    None
}

fn read_version_from_registry(component: &InstalledComponent) -> Option<String> {
    use crate::registry::RegistryManager;

    let manager = RegistryManager::for_component_type(component.component_type)?;
    let entries = manager.read_entries().ok()?;
    entries
        .iter()
        .find(|e| e.name == component.name || e.installed_path == component.path)
        .map(|e| e.version.clone())
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
