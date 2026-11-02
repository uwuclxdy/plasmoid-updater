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
#[cfg(test)]
mod testutil;

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use crate::{
    Failure, FailureKind, Reporter, registry,
    types::{AvailableUpdate, ComponentType, InstalledComponent},
};
use backup::{backup_component, restore_component};

use crate::version::normalize_version;

pub(crate) use backup::{BackupLedger, prune_backups};
pub(crate) use download::remove_stale_temp_dirs;
pub(crate) use inhibit::InhibitGuard;
pub(crate) use install::{is_installer_sibling, recover_interrupted_installs};
pub(crate) use lock::UpdateLock;
pub(crate) use plasmashell::{requires_restart, restart_plasmashell};

/// A single component's failure: the [`FailureKind`] naming the failed stage
/// plus a human-readable message. The run-level [`crate::Error`] is never
/// used for a per-component problem.
#[derive(Debug)]
pub(crate) struct ComponentFailure {
    pub kind: FailureKind,
    pub message: String,
}

impl ComponentFailure {
    pub(crate) fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn io(kind: FailureKind, path: &Path, source: std::io::Error) -> Self {
        Self::new(kind, format!("{}: {source}", path.display()))
    }

    pub(crate) fn into_failure(self, component: InstalledComponent) -> Failure {
        Failure {
            component,
            kind: self.kind,
            message: self.message,
        }
    }
}

/// Outcome of a single component update, including post-install verification.
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
/// Emits [`Event::Downloading`](crate::Event::Downloading) through `reporter`
/// as the download progresses. On failure the original component is restored
/// from its backup; if the restore itself fails, the returned
/// [`ComponentFailure`] carries kind
/// [`FailureKind::RestoreFailed`] and both messages. `backups` records the
/// backup's directory for the run's retention, which runs once the pool has
/// finished every component.
pub(crate) fn update_component(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
    reporter: &dyn Reporter,
    backups: &BackupLedger,
) -> Result<InstallOutcome, ComponentFailure> {
    let component = &update.installed;
    let temp = download::create_temp_dir()?;

    let backup_path = create_backup(component, backups)?;

    match perform_installation(update, client, temp.path(), reporter) {
        Ok(()) => {
            post_install_tasks(update)?;
            Ok(verify_installed_version(update))
        }
        Err(e) => {
            if let Some(ref backup) = backup_path {
                handle_installation_failure(backup, component, &e)?;
            }
            Err(e)
        }
    }
    // temp is dropped here, auto-cleanup
}

fn create_backup(
    component: &InstalledComponent,
    backups: &BackupLedger,
) -> Result<Option<PathBuf>, ComponentFailure> {
    backup_component(component, backups)
}

fn perform_installation(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
    temp_path: &Path,
    reporter: &dyn Reporter,
) -> Result<(), ComponentFailure> {
    let component = &update.installed;
    let downloaded_path = download::download_package(
        client,
        &update.download_url,
        update.checksum.as_deref(),
        component,
        temp_path,
        reporter,
    )?;

    execute_installation(
        &downloaded_path,
        component,
        &update.latest_version,
        temp_path,
    )
}

fn execute_installation(
    downloaded_path: &Path,
    component: &InstalledComponent,
    new_version: &str,
    temp_path: &Path,
) -> Result<(), ComponentFailure> {
    let is_single_file_type = matches!(
        component.component_type,
        ComponentType::ColorScheme | ComponentType::Wallpaper,
    );
    let is_raw_file = install::is_single_file_component(downloaded_path, component.component_type)
        || (is_single_file_type && !has_archive_magic(downloaded_path));

    if is_raw_file {
        let result = install::install_raw_file(downloaded_path, component);
        let _ = fs::remove_file(downloaded_path);
        result
    } else {
        install_from_archive(downloaded_path, component, new_version, temp_path)
    }
}

/// Checks the first bytes of a file for known archive format signatures.
fn has_archive_magic(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 6];
    let Ok(n) = file.read(&mut magic) else {
        return false;
    };
    if n < 2 {
        return false;
    }
    matches!(
        magic[..2],
        [0x1f, 0x8b]       // gzip
        | [b'P', b'K']     // zip
        | [b'B', b'Z']     // bzip2
    ) || (n >= 4 && magic[..4] == [0x28, 0xb5, 0x2f, 0xfd]) // zstd
        || (n >= 6 && magic[..6] == [0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00]) // xz
}

fn install_from_archive(
    downloaded_path: &Path,
    component: &InstalledComponent,
    new_version: &str,
    temp_path: &Path,
) -> Result<(), ComponentFailure> {
    let extract_dir = temp_path.join(format!("extract-{}", component.directory_name));

    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)
            .map_err(|e| ComponentFailure::io(FailureKind::Extract, &extract_dir, e))?;
    }

    if let Err(e) = download::extract_archive(downloaded_path, &extract_dir) {
        // For single-file types (color schemes, wallpapers), the download might
        // be the raw file itself served without the expected extension.
        if matches!(
            component.component_type,
            ComponentType::ColorScheme | ComponentType::Wallpaper
        ) {
            log::debug!(
                target: "extract",
                "extraction failed for {}, trying raw file install: {}",
                component.name,
                e.message,
            );
            let result = install::install_raw_file(downloaded_path, component);
            let _ = fs::remove_file(downloaded_path);
            return result;
        }
        let _ = fs::remove_file(downloaded_path);
        return Err(e);
    }

    let _ = fs::remove_file(downloaded_path);

    let result = if component.component_type.kpackage_type().is_some() {
        match install::install_via_kpackage(&extract_dir, component, new_version) {
            Ok(()) => Ok(()),
            Err(e) if component.component_type.has_direct_fallback() => {
                log::warn!(
                    target: "install",
                    "kpackagetool6 failed for {}, falling back to direct install: {}",
                    component.name,
                    e.message,
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

fn post_install_tasks(update: &AvailableUpdate) -> Result<(), ComponentFailure> {
    let component = &update.installed;

    let installed_json = component.path.join("metadata.json");
    let installed_desktop = component.path.join("metadata.desktop");

    if installed_json.exists() {
        if let Err(e) = install::patch_metadata(
            &installed_json,
            component.component_type,
            &update.latest_version,
        ) {
            log::warn!(target: "patch", "failed to update installed metadata: {}", e.message);
        }
    } else if installed_desktop.exists()
        && let Err(e) = install::patch_metadata_desktop(&installed_desktop, &update.latest_version)
    {
        log::warn!(target: "patch", "failed to update installed metadata.desktop: {}", e.message);
    }

    registry::update_registry_after_install(update).map_err(|e| {
        ComponentFailure::new(
            FailureKind::Registry,
            format!("installed, but the KNewStuff registry could not be updated: {e}"),
        )
    })
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
        .find(|e| {
            e.name == component.name
                || e.installed_path
                    .components()
                    .eq(component.path.components())
        })
        .map(|e| e.version.clone())
}

fn handle_installation_failure(
    backup_path: &Path,
    component: &InstalledComponent,
    original_error: &ComponentFailure,
) -> Result<(), ComponentFailure> {
    if let Err(restore_err) = restore_component(backup_path, &component.path) {
        log::error!(
            target: "restore",
            "failed to restore {:?} from its backup: {}",
            component.name,
            restore_err.message
        );
        Err(ComponentFailure::new(
            FailureKind::RestoreFailed,
            format!(
                "install failed ({}) and backup restore also failed ({})",
                original_error.message, restore_err.message
            ),
        ))
    } else {
        log::info!(target: "restore", "restored component from backup");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ComponentType;
    use std::sync::{Mutex, OnceLock};

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static RECORDER: Recorder = Recorder;

    struct Recorder;

    impl log::Log for Recorder {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            LINES.lock().unwrap().push(record.args().to_string());
        }

        fn flush(&self) {}
    }

    fn logged_by(run: impl FnOnce()) -> Vec<String> {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            log::set_logger(&RECORDER).unwrap();
            log::set_max_level(log::LevelFilter::Trace);
        });
        LINES.lock().unwrap().clear();
        run();
        LINES.lock().unwrap().clone()
    }

    fn color_scheme(path: &Path) -> InstalledComponent {
        InstalledComponent {
            name: "Fake Scheme".to_string(),
            directory_name: "FakeScheme.colors".to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::ColorScheme,
            path: path.to_path_buf(),
            is_system: false,
            release_date: String::new(),
        }
    }

    #[test]
    fn a_restore_failure_log_names_the_component() {
        let root = tempfile::tempdir().unwrap();
        let backup = root.path().join("backup");
        std::fs::write(&backup, b"old").unwrap();
        // A non-empty directory cannot take a file's rename, so the restore
        // fails the way a locked or full destination does.
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("occupant"), b"new").unwrap();
        let component = color_scheme(&target);
        let install_failure = ComponentFailure::new(FailureKind::Install, "install broke");

        let lines = logged_by(|| {
            let outcome = handle_installation_failure(&backup, &component, &install_failure);
            assert!(outcome.is_err(), "the restore was expected to fail");
        });

        assert!(
            lines.iter().any(|line| line.contains(&component.name)),
            "the restore failure log does not name the component: {lines:?}",
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&format!("{:?}", component.name))),
            "the restore failure log does not repr the component name: {lines:?}",
        );
    }
}
