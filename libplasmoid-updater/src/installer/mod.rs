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
pub(crate) use install::{
    MAX_METADATA_BYTES, has_interrupted_installs, is_installer_sibling, is_regular_file,
    metadata_size_ok, recover_interrupted_installs, stranded_components,
};
pub(crate) use lock::{UpdateLock, run_lock_held};
pub(crate) use plasmashell::{requires_restart, restart_plasmashell};

/// A single component's failure: the [`FailureKind`] naming the failed stage
/// plus a human-readable message. The run-level [`crate::Error`] is never
/// used for a per-component problem.
#[derive(Debug)]
pub(crate) struct ComponentFailure {
    pub kind: FailureKind,
    pub message: String,
    /// A process the failed step started may still be writing the component's
    /// tree, so restoring the backup would race it.
    pub process_may_still_run: bool,
    /// The operating system's error, when the failure is one it named. A
    /// caller that must key on the error's kind reads it here instead of from
    /// the rendered message.
    source: Option<std::io::Error>,
}

impl ComponentFailure {
    pub(crate) fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            process_may_still_run: false,
            source: None,
        }
    }

    pub(crate) fn io(kind: FailureKind, path: &Path, source: std::io::Error) -> Self {
        Self {
            kind,
            message: format!("{}: {source}", path.display()),
            process_may_still_run: false,
            source: Some(source),
        }
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
/// from its backup unless the failed step left a process that may still write
/// that tree; if the restore itself fails, the returned
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
    validate_selected_update(update)?;
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

/// Refuses a selected update with no download URL before a backup or a
/// download starts, so the rest of the selection continues. An empty version
/// is a store value (`<version>` may be empty) and installs like any other.
fn validate_selected_update(update: &AvailableUpdate) -> Result<(), ComponentFailure> {
    if update.download_url.is_empty() {
        return Err(ComponentFailure::new(
            FailureKind::NoMatchingDownload,
            "the selected update has no download url",
        ));
    }
    Ok(())
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
    let downloaded_path = download::download_package(update, client, temp_path, reporter)?;

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
    // A raw file is the component itself only when the download carries no
    // archive signature at all; an archive that fails to extract is an extract
    // failure, never a component file.
    let is_raw_file = is_single_file_type && !has_archive_magic(downloaded_path);

    if is_raw_file {
        let result = install::install_raw_file(downloaded_path, component);
        let _ = fs::remove_file(downloaded_path);
        result
    } else {
        install_from_archive(downloaded_path, component, new_version, temp_path)
    }
}

/// Checks the first bytes of a file for known archive format signatures.
///
/// A plain tar stores `ustar` 257 bytes in, so the probe reads a whole header
/// rather than only the leading magic.
fn has_archive_magic(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 263];
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
        || (n >= 262 && &magic[257..262] == b"ustar") // tar (ustar/gnu)
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
        let _ = fs::remove_file(downloaded_path);
        return Err(e);
    }

    let _ = fs::remove_file(downloaded_path);

    // The package must be a regular package of the component the store entry
    // claims before any install step touches the installed tree: kpackagetool6
    // follows every symlink in the package and blocks on a FIFO, and a copy
    // would read whatever a link points at. The package root's own metadata is
    // named first, so its refusal names the file rather than the link in it.
    let result = install::verify_extracted_package(&extract_dir)
        .and_then(|()| install::verify_package_tree(&extract_dir, component.component_type))
        .and_then(|()| install::verify_package_plugin_id(&extract_dir, component))
        .and_then(|()| {
            if component.component_type.kpackage_type().is_some() {
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
            }
        });

    let _ = fs::remove_dir_all(&extract_dir);
    result
}

fn post_install_tasks(update: &AvailableUpdate) -> Result<(), ComponentFailure> {
    let component = &update.installed;

    let installed_json = component.path.join("metadata.json");
    let installed_desktop = component.path.join("metadata.desktop");

    if install::is_regular_file(&installed_json) {
        if let Err(e) = install::patch_metadata(
            &installed_json,
            component.component_type,
            &update.latest_version,
        ) {
            log::warn!(target: "patch", "failed to update installed metadata: {}", e.message);
        }
    } else if install::is_regular_file(&installed_desktop)
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
    verify_installed_version_trusting(
        update,
        crate::paths::system_data_dirs(update.installed.is_system),
        &crate::paths::knewstuff_dir(),
        crate::paths::is_trusted_system_path,
    )
}

/// `system_roots`, `user_knewstuff` and `trusted` decide which registry the
/// version read uses, so a test can point a `--system` read at a trusted
/// fixture a tempdir root could never make pass the real trust check.
fn verify_installed_version_trusting(
    update: &AvailableUpdate,
    system_roots: &[PathBuf],
    user_knewstuff: &Path,
    trusted: fn(&Path) -> bool,
) -> InstallOutcome {
    let component = &update.installed;
    let expected = &update.latest_version;

    let actual = read_installed_version_trusting(component, system_roots, user_knewstuff, trusted);

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

fn read_installed_version_trusting(
    component: &InstalledComponent,
    system_roots: &[PathBuf],
    user_knewstuff: &Path,
    trusted: fn(&Path) -> bool,
) -> Option<String> {
    // For registry-only types, read from the KNewStuff registry the update
    // wrote: the user registry for a user component, the trusted system
    // registry for a system one.
    if component.component_type.registry_only() {
        return crate::registry::read_component_version_trusting(
            component,
            system_roots,
            user_knewstuff,
            trusted,
        );
    }

    // Try metadata.json first
    let json_path = component.path.join("metadata.json");
    if install::is_regular_file(&json_path) {
        if !install::metadata_size_ok(&json_path) {
            log::warn!(
                target: "verify",
                "ignoring {}: larger than the {}-byte metadata cap",
                json_path.display(),
                install::MAX_METADATA_BYTES
            );
        } else if let Ok(content) = fs::read_to_string(&json_path)
            && let Ok(meta) = serde_json::from_str::<crate::types::PackageMetadata>(&content)
            && let Some(v) = meta.version()
        {
            return Some(v.to_string());
        }
    }

    // Fall back to metadata.desktop
    let desktop_path = component.path.join("metadata.desktop");
    if install::is_regular_file(&desktop_path)
        && install::metadata_size_ok(&desktop_path)
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

/// Puts the backup back after a failed install, unless a process the failure
/// left behind may still be writing the component's tree.
///
/// A tool that survived the kill keeps the tree open; restoring over it would
/// mix the old and new trees under a writer, so the failure stands, names the
/// backup to restore from, and leaves it for the next run.
fn handle_installation_failure(
    backup_path: &Path,
    component: &InstalledComponent,
    original_error: &ComponentFailure,
) -> Result<(), ComponentFailure> {
    if original_error.process_may_still_run {
        log::error!(
            target: "restore",
            "not restoring {:?}: a process the failed install started may still be writing it; the backup is at {}",
            component.name,
            backup_path.display()
        );
        return Err(ComponentFailure::new(
            FailureKind::Install,
            format!(
                "install failed ({}), and the tool that failed it may still be running; the backup was kept at {}",
                original_error.message,
                backup_path.display()
            ),
        ));
    }
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
    use crate::testutil::logs::captured_logs_while;
    use crate::types::ComponentType;

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

        let lines = captured_logs_while(|| {
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
        // The buffer is shared with the other tests in this process, so the
        // assertion above can be met by a line another test logged in the same
        // window; this one names the message only the failed restore emits.
        assert!(
            lines.iter().any(|line| line.contains("failed to restore")
                && line.contains(&format!("{:?}", component.name))),
            "the restore failure log does not name the failed restore: {lines:?}",
        );
    }

    #[test]
    fn a_failure_that_left_a_process_running_keeps_the_backup() {
        let root = tempfile::tempdir().unwrap();
        let backup = root.path().join("backup");
        std::fs::write(&backup, b"old").unwrap();
        let target = root.path().join("target");
        std::fs::write(&target, b"new").unwrap();
        let component = color_scheme(&target);
        let mut failure = ComponentFailure::new(
            FailureKind::Install,
            "kpackagetool6 timed out after 300 s and is still running",
        );
        failure.process_may_still_run = true;

        let lines = captured_logs_while(|| {
            let refusal = handle_installation_failure(&backup, &component, &failure).unwrap_err();
            assert_eq!(refusal.kind, FailureKind::Install);
            assert!(
                refusal.message.contains(&backup.display().to_string()),
                "{}",
                refusal.message
            );
            assert!(
                refusal.message.contains("still running"),
                "{}",
                refusal.message
            );
        });

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "new",
            "the backup was restored over a tree a running tool may still be writing"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("not restoring") && line.contains(&component.name)),
            "the kept backup was not logged: {lines:?}",
        );
    }

    fn plain_tar_holding(dir: &Path, file: &str, content: &str) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(file), content).unwrap();
        let archive = dir.join("package");
        let status = std::process::Command::new("tar")
            .arg("--create")
            .arg("--format=ustar")
            .arg("--file")
            .arg(&archive)
            .arg("--directory")
            .arg(&src)
            .arg(file)
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the fixture");
        archive
    }

    #[test]
    fn a_plain_tar_counts_as_an_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = plain_tar_holding(dir.path(), "Scheme.colors", "[Colors:View]\n");

        assert!(has_archive_magic(&archive));
    }

    #[test]
    fn a_plain_tar_color_scheme_installs_its_member_not_the_tar() {
        let root = tempfile::tempdir().unwrap();
        let archive = plain_tar_holding(root.path(), "Scheme.colors", "[Colors:View]\n# new\n");
        let dest = root.path().join("installed/FakeScheme.colors");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "[Colors:View]\n# old\n").unwrap();
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        execute_installation(&archive, &color_scheme(&dest), "2.0", &temp).unwrap();

        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "[Colors:View]\n# new\n"
        );
    }

    #[test]
    fn a_corrupt_archive_fails_extraction_instead_of_installing_its_bytes() {
        let root = tempfile::tempdir().unwrap();
        // gzip magic followed by bytes no gzip reader accepts.
        let archive = root.path().join("package.tar.gz");
        std::fs::write(&archive, [0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad, 0xbe, 0xef]).unwrap();
        let dest = root.path().join("installed/FakeScheme.colors");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "[Colors:View]\n# old\n").unwrap();
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let result = execute_installation(&archive, &color_scheme(&dest), "2.0", &temp);

        let failure = result.expect_err("a corrupt archive installed");
        assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "[Colors:View]\n# old\n"
        );
    }

    fn system_scheme_update(path: PathBuf, latest: &str, content_id: u64) -> AvailableUpdate {
        AvailableUpdate::builder(
            InstalledComponent::new(
                "Fake Scheme".to_string(),
                "FakeScheme.colors".to_string(),
                ComponentType::ColorScheme,
                path,
            )
            .with_version("1.0")
            .with_system(true),
            content_id,
            latest,
        )
        .download_url("https://example.test/dl")
        .release_date("2026-02-02")
        .build()
    }

    fn write_scheme_registry(registry_dir: &Path, version: &str, installed: &Path) {
        std::fs::create_dir_all(registry_dir).unwrap();
        std::fs::write(
            registry_dir.join("colorschemes.knsregistry"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Fake Scheme</name>
    <version>{version}</version>
    <installedfile>{installed}</installedfile>
    <id>42</id>
  </stuff>
</hotnewstuffregistry>
"#,
                installed = installed.display(),
            ),
        )
        .unwrap();
    }

    /// The post-install version read for a `--system` registry-only component
    /// reads the trusted system registry the update wrote; a planted user
    /// registry entry naming the same component must not decide the outcome.
    #[test]
    fn a_system_registry_version_read_reports_the_update_verified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let installed = PathBuf::from("/usr/share/color-schemes/FakeScheme.colors");
        let update = system_scheme_update(installed.clone(), "2.0", 42);

        let system = root.join("system");
        write_scheme_registry(&system.join("knewstuff3"), "2.0", &installed);
        let user = root.join("user-knewstuff3");
        write_scheme_registry(&user, "9.9", &installed);

        let outcome = verify_installed_version_trusting(
            &update,
            std::slice::from_ref(&system),
            &user,
            |_| true,
        );

        assert!(outcome.verified, "the update read as unverified");
        assert_eq!(outcome.actual_version.as_deref(), Some("2.0"));
    }

    fn user_scheme_update(path: PathBuf, latest: &str, content_id: u64) -> AvailableUpdate {
        AvailableUpdate::builder(
            InstalledComponent::new(
                "Fake Scheme".to_string(),
                "FakeScheme.colors".to_string(),
                ComponentType::ColorScheme,
                path,
            )
            .with_version("1.0"),
            content_id,
            latest,
        )
        .download_url("https://example.test/dl")
        .release_date("2026-02-02")
        .build()
    }

    /// The production version read supplies the run's own registry inputs: the
    /// user registry from `XDG_DATA_HOME` and the system roots from
    /// `XDG_DATA_DIRS`, whose user-owned entries the real trust check drops.
    #[test]
    fn the_production_version_read_takes_its_inputs_from_the_environment() {
        const CHILD: &str = "PU_TEST_VERSION_READ_CHILD";
        const ROOT: &str = "PU_TEST_VERSION_READ_ROOT";

        if std::env::var_os(CHILD).is_some() {
            crate::testutil::logs::capture_logs();
            let root = PathBuf::from(std::env::var(ROOT).unwrap());
            let share = root.join("share");
            let data = root.join("data");

            let user_path = root.join("installed/FakeScheme.colors");
            std::fs::create_dir_all(user_path.parent().unwrap()).unwrap();
            write_scheme_registry(&data.join("knewstuff3"), "2.0", &user_path);
            let outcome = verify_installed_version(&user_scheme_update(user_path, "2.0", 42));
            assert!(
                outcome.verified,
                "the user registry under XDG_DATA_HOME was not read: {:?}",
                outcome.actual_version
            );

            let system_path = PathBuf::from("/usr/share/color-schemes/FakeScheme.colors");
            let outcome = verify_installed_version(&system_scheme_update(system_path, "2.0", 42));
            assert!(!outcome.verified);
            let logs = crate::testutil::logs::captured_logs();
            assert!(
                logs.iter().any(|line| line.starts_with("paths: ")
                    && line.contains(&share.display().to_string())),
                "the system roots did not come from XDG_DATA_DIRS: {logs:?}"
            );

            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("share")).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::tests::the_production_version_read_takes_its_inputs_from_the_environment",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_DATA_DIRS", root.join("share"))
            .env(CHILD, "1")
            .env(ROOT, &root)
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
    }
}
