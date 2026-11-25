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
mod record;

use std::{
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
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
pub(crate) use record::{
    RecordRows, RecordSites, record_rows, repair as repair_records,
    report_lost as report_lost_records, spared_backups,
};

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
    /// `bsdtar` named no archive format for the file it was asked to list; a
    /// single-file component's download can be the component file itself only
    /// when this answer holds and the file carries no archive signature.
    pub unrecognized_format: bool,
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
            unrecognized_format: false,
            source: None,
        }
    }

    pub(crate) fn io(kind: FailureKind, path: &Path, source: std::io::Error) -> Self {
        Self {
            kind,
            message: format!("{}: {source}", path.display()),
            process_may_still_run: false,
            unrecognized_format: false,
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

/// The run's one install slot.
///
/// The install record, backup, install, metadata patch, registry write,
/// version check and restore steps take a reference to the guard
/// [`Self::acquire`] hands out, so calling one of them without a live guard
/// does not compile; the functions they delegate to take none.
pub(crate) struct InstallSlot {
    held: Mutex<()>,
}

impl InstallSlot {
    pub(crate) fn new() -> Self {
        Self {
            held: Mutex::new(()),
        }
    }

    /// Takes the slot for as long as the returned guard lives.
    ///
    /// The slot guards no data, only the order of installs, so a panic in
    /// another component's install leaves nothing for this one to distrust.
    ///
    /// No rayon call may run while the guard lives: the pool's worker that
    /// holds it would steal another component's job while it waits, and that
    /// job's `acquire` on the same thread never returns.
    pub(crate) fn acquire(&self) -> InstallSlotGuard<'_> {
        InstallSlotGuard {
            _held: self.held.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }
}

/// Holds the run's install slot for as long as it lives.
///
/// The steps [`InstallSlot`] names take a reference to this guard, so calling
/// one of them after the guard is dropped does not compile.
pub(crate) struct InstallSlotGuard<'a> {
    _held: MutexGuard<'a, ()>,
}

/// Updates a single component using the provided HTTP client.
///
/// The download runs first, emitting
/// [`Event::Downloading`](crate::Event::Downloading) through `reporter` as it
/// progresses, then the download is unpacked and checked and what the
/// component's type installs by is resolved from it; all of that writes only
/// the component's own temp dir and needs nothing from the other components. A
/// failure there leaves the component untouched, so it takes no backup and has
/// nothing restored over it. The backup, the install, the registry write and
/// the version check then run while holding the [`InstallSlot`]'s guard, one
/// component at a time across the run: components of one type share a
/// `.knsregistry` file, and each install reads, edits and rewrites it. The
/// backup and the install also write into directories other components share:
/// the backup timestamp directory every type backed up in the same second
/// writes into, and the `/usr/share/icons` tree that system-wide icon and
/// cursor themes both install into.
///
/// Under the guard, before the backup, an install record an interrupted
/// install left beside the component is settled first (see
/// [`record::settle_prior`]); after the backup, the component's record names
/// it until the install settles, so a run killed mid-install is repaired from
/// it by the next run. The record goes once the files are in place, before the
/// registry write, and once a failed install is restored; it stays while a
/// process the failed step started may still run, or when the restore failed. A
/// record that cannot be removed once the files are in place fails the
/// component before the registry write, so a later repair cannot put the old
/// files back over a run it reported as landed.
///
/// When the install fails, the original component is restored from its backup,
/// still under the guard, unless the failed step left a process that may still
/// write that tree; if the restore itself fails, the returned
/// [`ComponentFailure`] carries kind [`FailureKind::RestoreFailed`] and both
/// messages. `backups` records the backup's directory for the run's retention,
/// which runs once the pool has finished every component.
pub(crate) fn update_component(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
    reporter: &dyn Reporter,
    backups: &BackupLedger,
    install_slot: &InstallSlot,
    sites: &RecordSites,
) -> Result<InstallOutcome, ComponentFailure> {
    validate_selected_update(update)?;
    let component = &update.installed;
    let temp = download::create_temp_dir()?;
    let downloaded = download::download_package(update, client, temp.path(), reporter)?;
    let served_name = download::served_file_name(&update.download_url);
    let served_name = served_name.as_deref().map_err(String::as_str);
    let unpacked = unpack_download(
        &downloaded,
        served_name,
        component,
        &update.latest_version,
        temp.path(),
    )?;

    let slot = install_slot.acquire();
    let prior = record::settle_prior(sites, component, &slot)?;
    let backup_path = backup_component(component, backups, &slot)?;
    let record = record::Open::write(sites, component, backup_path.as_deref(), prior, &slot);

    match execute_installation(&unpacked, component, &slot) {
        Ok(()) => {
            // A rebuilt wallpaper keeps its `metadata.desktop` as it was, so
            // only its `metadata.json` takes the new version, and only when the
            // rebuild kept one: any other `metadata.json` in the folder is the
            // image the store served.
            match &unpacked {
                Unpacked::RebuiltWallpaper {
                    kept_json: true, ..
                } => patch_rebuilt_metadata(component, &update.latest_version, &slot),
                Unpacked::RebuiltWallpaper { .. } => {}
                _ => patch_installed_metadata(update, &slot),
            }
            record.landed(sites, &slot)?;
            write_registry_entry(update, &slot)?;
            Ok(verify_installed_version(update, &slot))
        }
        Err(e) => {
            if let Some(backup) = &backup_path {
                handle_installation_failure(backup, component, &e, &slot)?;
                record.restored(sites, &slot);
            }
            Err(e)
        }
    }
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

/// What a download installs, resolved from the unpacked tree before the
/// install slot is taken, so a lookup that fails here takes no backup.
#[derive(Debug)]
enum Unpacked {
    /// A single-file component's download that is the component file itself.
    RawFile(PathBuf),
    /// The directory a wallpaper installed as one becomes around the single
    /// image its download holds, and whether it kept the installed
    /// `metadata.json`.
    RebuiltWallpaper {
        tree: PathBuf,
        /// Whether the installed `metadata.json` is in `tree`: one kept there
        /// is the component's metadata, and only it takes the new version.
        kept_json: bool,
    },
    /// The file or directory a direct install copies into the component's path.
    Direct(install::DirectSource),
    /// A package without a direct fallback, its metadata patched for the new
    /// version: `kpackagetool6` installs it, unless its plugin id names another
    /// directory, which the install copies into place instead.
    Package(PathBuf),
    /// The extracted tree of a type that falls back to a direct install when
    /// `kpackagetool6` fails. Its direct install source stays unresolved,
    /// because the tool may have written the component by the time the direct
    /// install runs.
    Fallback {
        tree: PathBuf,
        /// The prepared package root, or the refusal preparing it hit; the
        /// direct install decides then, and `kpackagetool6` never runs.
        package: Result<PathBuf, ComponentFailure>,
    },
}

/// Decides what the download installs, extracting and checking an archive and
/// resolving what the component's type installs by, writing only
/// `temp_path`. `served_name` is the file name the store served the download
/// under, or its spelling as served when its escapes do not decode to UTF-8.
/// A single-file component's zero-byte download fails here, naming what the
/// store served, before anything is extracted or backed up.
///
/// A wallpaper installed as a directory whose download is one image, raw or
/// the only thing its archive holds, is rebuilt here around that image (see
/// [`install::rebuild_wallpaper_dir`]): the installed shape decides, so the
/// update lands on the path the desktop config and the install record name.
fn unpack_download(
    downloaded_path: &Path,
    served_name: Result<&str, &str>,
    component: &InstalledComponent,
    new_version: &str,
    temp_path: &Path,
) -> Result<Unpacked, ComponentFailure> {
    let is_single_file_type = matches!(
        component.component_type,
        ComponentType::ColorScheme | ComponentType::Wallpaper,
    );
    // A zero-byte download is neither the component file nor an archive:
    // `bsdtar` lists it as an empty archive, so the single-file lookups would
    // name it one. It fails here, before anything is extracted or backed up.
    if is_single_file_type
        && fs::metadata(downloaded_path)
            .map_err(|e| ComponentFailure::io(FailureKind::Extract, downloaded_path, e))?
            .len()
            == 0
    {
        return Err(ComponentFailure::new(
            FailureKind::Install,
            "the store served an empty file",
        ));
    }
    let extract_dir = temp_path.join(format!("extract-{}", component.directory_name));
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)
            .map_err(|e| ComponentFailure::io(FailureKind::Extract, &extract_dir, e))?;
    }
    let extracted = download::extract_archive(downloaded_path, &extract_dir);
    // A single-file component's download is the component file itself only
    // when `bsdtar` names no archive format for it and the file carries no
    // archive signature: `bsdtar` names none for a truncated or header-damaged
    // archive either, so its signature makes it fail as an extract failure
    // instead of being installed as the component file.
    if is_single_file_type
        && let Err(failure) = &extracted
        && failure.unrecognized_format
        && !has_archive_magic(downloaded_path)
    {
        if install::is_directory_wallpaper(component) {
            return rebuilt_wallpaper(
                downloaded_path,
                install::ImageName::Served(served_name),
                component,
                temp_path,
            );
        }
        return Ok(Unpacked::RawFile(downloaded_path.to_path_buf()));
    }
    // The archive is spent either way; removing it now frees its space while
    // the extracted tree waits for the install slot.
    if let Err(e) = fs::remove_file(downloaded_path) {
        log::debug!(
            target: "cleanup",
            "failed to remove {}: {e}",
            downloaded_path.display()
        );
    }
    extracted?;

    // The package must be a regular package of the component the store entry
    // claims before any install step touches the installed tree: kpackagetool6
    // follows every symlink in the package and blocks on a FIFO, and a copy
    // would read whatever a link points at. The package root's own metadata is
    // named first, so its refusal names the file rather than the link in it.
    install::verify_extracted_package(&extract_dir)?;
    install::verify_package_tree(&extract_dir, component.component_type)?;
    // A loose image carries no plugin id to compare; the rebuilt directory
    // keeps the installed metadata, and with it the installed id.
    if install::is_directory_wallpaper(component)
        && let Some(image) = install::loose_wallpaper_image(&extract_dir)
        && let Some(image_name) = image.file_name()
    {
        return rebuilt_wallpaper(
            &image,
            install::ImageName::Member(image_name),
            component,
            temp_path,
        );
    }
    install::verify_package_plugin_id(&extract_dir, component)?;
    match component.component_type.kpackage_type() {
        None => Ok(Unpacked::Direct(install::resolve_direct_source(
            &extract_dir,
            component,
        )?)),
        Some(_) if component.component_type.has_direct_fallback() => {
            // `kpackagetool6` may write the component before the direct
            // fallback runs, so a package route that refuses on its own does
            // not fail the update. A package naming no plugin id is one the
            // tool never installs, so that route is certain to fail for it
            // too. When no route can install, the pair refuses here, before a
            // backup exists.
            let package = install::prepare_package(&extract_dir, component, new_version);
            let package_route_refuses = match &package {
                Err(_) => true,
                Ok(package) => install::plugin_id(package).is_none(),
            };
            if package_route_refuses {
                install::resolve_direct_source(&extract_dir, component)?;
            }
            Ok(Unpacked::Fallback {
                tree: extract_dir,
                package,
            })
        }
        Some(_) => Ok(Unpacked::Package(install::prepare_package(
            &extract_dir,
            component,
            new_version,
        )?)),
    }
}

/// Builds the directory a wallpaper installed as one becomes around the image
/// its download holds, and hands it back as what the install swaps in.
fn rebuilt_wallpaper(
    image: &Path,
    image_name: install::ImageName<'_>,
    component: &InstalledComponent,
    temp_path: &Path,
) -> Result<Unpacked, ComponentFailure> {
    let tree = temp_path.join(format!("rebuilt-{}", component.directory_name));
    let kept_json = install::rebuild_wallpaper_dir(image, image_name, &component.path, &tree)?;
    Ok(Unpacked::RebuiltWallpaper { tree, kept_json })
}

/// Installs what [`unpack_download`] made ready. The run's temp dir, which
/// holds it, is removed when the update returns.
fn execute_installation(
    unpacked: &Unpacked,
    component: &InstalledComponent,
    _slot: &InstallSlotGuard<'_>,
) -> Result<(), ComponentFailure> {
    match unpacked {
        Unpacked::RawFile(file) => install::install_raw_file(file, component),
        Unpacked::RebuiltWallpaper { tree, .. } => {
            install::install_wallpaper(tree, &component.path)
        }
        Unpacked::Direct(source) => install::install_direct(source, component),
        Unpacked::Package(package_dir) => install::install_prepared_package(package_dir, component),
        Unpacked::Fallback { tree, package } => {
            let installed = match package {
                Ok(package) => install::install_prepared_package(package, component),
                Err(refused) => {
                    log::warn!(
                        target: "install",
                        "{:?}: installing its files directly, the package could not be used: {:?}",
                        component.name,
                        refused.message,
                    );
                    return install::install_direct(
                        &install::resolve_direct_source(tree, component)?,
                        component,
                    );
                }
            };
            match installed {
                Ok(()) => Ok(()),
                // A tool the timeout could not prove gone may still be writing
                // the tree, so nothing may be copied over it.
                Err(e) if e.process_may_still_run => Err(e),
                Err(e) => {
                    log::warn!(
                        target: "install",
                        "{:?}: falling back to a direct install: {:?}",
                        component.name,
                        e.message,
                    );
                    install::install_direct(
                        &install::resolve_direct_source(tree, component)?,
                        component,
                    )
                }
            }
        }
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

/// Writes the new version into the installed metadata, warning when it cannot.
fn patch_installed_metadata(update: &AvailableUpdate, _slot: &InstallSlotGuard<'_>) {
    let component = &update.installed;

    let installed_json = component.path.join("metadata.json");
    let installed_desktop = component.path.join("metadata.desktop");

    if install::is_regular_file(&installed_json) {
        patch_json_metadata(
            &installed_json,
            component.component_type,
            &update.latest_version,
        );
    } else if install::is_regular_file(&installed_desktop)
        && let Err(e) = install::patch_metadata_desktop(&installed_desktop, &update.latest_version)
    {
        log::warn!(target: "patch", "failed to update installed metadata.desktop: {}", e.message);
    }
}

/// Writes the new version into a rebuilt wallpaper's installed `metadata.json`,
/// warning when it cannot. The directory keeps its `metadata.desktop`, where it
/// has one, byte for byte.
fn patch_rebuilt_metadata(
    component: &InstalledComponent,
    new_version: &str,
    _slot: &InstallSlotGuard<'_>,
) {
    patch_json_metadata(
        &component.path.join("metadata.json"),
        component.component_type,
        new_version,
    );
}

/// Writes the new version into a `metadata.json`, warning when it cannot.
fn patch_json_metadata(path: &Path, component_type: ComponentType, new_version: &str) {
    if let Err(e) = install::patch_metadata(path, component_type, new_version) {
        log::warn!(target: "patch", "failed to update installed metadata: {}", e.message);
    }
}

fn write_registry_entry(
    update: &AvailableUpdate,
    _slot: &InstallSlotGuard<'_>,
) -> Result<(), ComponentFailure> {
    registry::update_registry_after_install(update).map_err(|e| {
        ComponentFailure::new(
            FailureKind::Registry,
            format!("installed, but the KNewStuff registry could not be updated: {e}"),
        )
    })
}

fn verify_installed_version(
    update: &AvailableUpdate,
    _slot: &InstallSlotGuard<'_>,
) -> InstallOutcome {
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
    _slot: &InstallSlotGuard<'_>,
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
    use super::install::tests::{run_child, write_script};
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
        let slot = InstallSlot::new();

        let lines = captured_logs_while(|| {
            let outcome =
                handle_installation_failure(&backup, &component, &install_failure, &slot.acquire());
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
        let slot = InstallSlot::new();

        let lines = captured_logs_while(|| {
            let refusal =
                handle_installation_failure(&backup, &component, &failure, &slot.acquire())
                    .unwrap_err();
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

    fn wallpaper(path: &Path) -> InstalledComponent {
        InstalledComponent {
            name: "Fake Wallpaper".to_string(),
            directory_name: "FakeWallpaper.png".to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::Wallpaper,
            path: path.to_path_buf(),
            is_system: false,
            release_date: String::new(),
        }
    }

    /// Builds a tar of `format` at `dir/package` holding `file`.
    fn tar_holding(dir: &Path, format: &str, file: &str, content: &str) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(file), content).unwrap();
        let archive = dir.join("package");
        let status = std::process::Command::new("tar")
            .args(["--create", "--format", format, "--file"])
            .arg(&archive)
            .arg("--directory")
            .arg(&src)
            .arg(file)
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the fixture");
        archive
    }

    /// Installs `download` as the `component`, expecting `bsdtar`'s refusal to
    /// make it the component file itself.
    fn install_raw_download(download: &Path, component: &InstalledComponent, temp: &Path) {
        let unpacked = unpack_download(download, Ok("download"), component, "2.0", temp).unwrap();
        assert!(
            matches!(unpacked, Unpacked::RawFile(_)),
            "the download was not taken as the component file itself"
        );
        let slot = InstallSlot::new();
        execute_installation(&unpacked, component, &slot.acquire()).unwrap();
    }

    #[test]
    fn a_plain_tar_counts_as_an_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_holding(dir.path(), "ustar", "Scheme.colors", "[Colors:View]\n");

        assert!(has_archive_magic(&archive));
    }

    #[test]
    fn a_plain_tar_color_scheme_installs_its_member_not_the_tar() {
        let root = tempfile::tempdir().unwrap();
        let archive = tar_holding(
            root.path(),
            "ustar",
            "Scheme.colors",
            "[Colors:View]\n# new\n",
        );
        let dest = root.path().join("installed/FakeScheme.colors");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "[Colors:View]\n# old\n").unwrap();
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let component = color_scheme(&dest);
        let unpacked = unpack_download(&archive, Ok("download"), &component, "2.0", &temp).unwrap();
        let slot = InstallSlot::new();
        execute_installation(&unpacked, &component, &slot.acquire()).unwrap();

        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "[Colors:View]\n# new\n"
        );
    }

    /// A v7 tar carries no `ustar` mark, so a header probe misses it while
    /// `bsdtar` reads it as the archive it is.
    #[test]
    fn a_v7_tar_color_scheme_installs_its_member_not_the_tar() {
        let root = tempfile::tempdir().unwrap();
        let archive = tar_holding(root.path(), "v7", "Scheme.colors", "[Colors:View]\n# new\n");
        let dest = root.path().join("installed/FakeScheme.colors");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "[Colors:View]\n# old\n").unwrap();
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let component = color_scheme(&dest);
        let unpacked = unpack_download(&archive, Ok("download"), &component, "2.0", &temp).unwrap();
        let slot = InstallSlot::new();
        execute_installation(&unpacked, &component, &slot.acquire()).unwrap();

        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "[Colors:View]\n# new\n"
        );
    }

    #[test]
    fn a_raw_color_scheme_installs_as_the_component_file() {
        let root = tempfile::tempdir().unwrap();
        let download = root.path().join("download");
        std::fs::write(&download, "[General]\nName=Raw\n").unwrap();
        let dest = root.path().join("installed/FakeScheme.colors");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        install_raw_download(&download, &color_scheme(&dest), &temp);

        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "[General]\nName=Raw\n"
        );
    }

    #[test]
    fn a_raw_wallpaper_installs_as_the_component_file() {
        let root = tempfile::tempdir().unwrap();
        let download = root.path().join("download");
        // A PNG signature and the start of its IHDR chunk: bytes no archive
        // format bids on.
        std::fs::write(
            &download,
            [
                0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H',
                b'D', b'R',
            ],
        )
        .unwrap();
        let dest = root.path().join("installed/FakeWallpaper.png");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        install_raw_download(&download, &wallpaper(&dest), &temp);

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            std::fs::read(&download).unwrap(),
            "the raw image was not installed byte for byte"
        );
    }

    /// An archive holding no members, which `bsdtar` lists empty.
    fn empty_tar(dir: &Path) -> PathBuf {
        let archive = dir.join("empty.tar");
        let status = std::process::Command::new("tar")
            .args(["--create", "--file"])
            .arg(&archive)
            .args(["--files-from", "/dev/null"])
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the empty fixture");
        archive
    }

    /// A zero-byte download is neither the component file nor an archive: it
    /// fails with the line naming what the store served.
    #[test]
    fn an_empty_download_fails_the_color_scheme() {
        let root = tempfile::tempdir().unwrap();
        let download = root.path().join("download");
        std::fs::write(&download, b"").unwrap();
        let dest = root.path().join("installed/FakeScheme.colors");
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let failure = unpack_download(
            &download,
            Ok("download"),
            &color_scheme(&dest),
            "2.0",
            &temp,
        )
        .expect_err("an empty download installed as a component file");

        assert_eq!(failure.kind, FailureKind::Install, "{}", failure.message);
        assert_eq!(failure.message, "the store served an empty file");
    }

    /// An archive that lists empty keeps the lookup's own line: only a
    /// zero-byte download is named as the store's empty file.
    #[test]
    fn an_empty_archive_fails_the_color_scheme_lookup() {
        let root = tempfile::tempdir().unwrap();
        let archive = empty_tar(root.path());
        let dest = root.path().join("installed/FakeScheme.colors");
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let failure = unpack_download(&archive, Ok("download"), &color_scheme(&dest), "2.0", &temp)
            .expect_err("an empty archive installed as the component file");

        assert_eq!(failure.kind, FailureKind::Install, "{}", failure.message);
        assert_eq!(failure.message, "no color scheme file found in archive");
    }

    const NEAR_MISS_LISTING_CHILD: &str =
        "installer::tests::a_listing_answer_that_is_not_the_exact_refusal_fails_the_extract";

    /// A fake `bsdtar` whose refusal holds the words of the real one and more,
    /// so a match wider than the whole line would take a download as raw.
    const NEAR_MISS_LISTING_BSDTAR: &str = r#"#!/bin/sh
printf '%s\n' 'bsdtar: Error opening archive: Unrecognized archive format; retry' >&2
exit 1
"#;

    #[test]
    fn a_listing_answer_that_is_not_the_exact_refusal_fails_the_extract() {
        if std::env::var_os("PU_TEST_NEAR_MISS_LISTING").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let component = color_scheme(&root.join("installed/FakeScheme.colors"));

            let failure = unpack_download(
                &root.join("download"),
                Ok("download"),
                &component,
                "2.0",
                &root.join("work"),
            )
            .expect_err(
                "a listing answer that is not the exact refusal installed the download raw",
            );

            assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
            assert_eq!(
                failure.message,
                "bsdtar: Error opening archive: Unrecognized archive format; retry"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), NEAR_MISS_LISTING_BSDTAR);
        std::fs::write(root.join("download"), "[General]\nName=Raw\n").unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        let done = root.join("child-done");

        let output = run_child(
            NEAR_MISS_LISTING_CHILD,
            &bin,
            &[
                ("PU_TEST_NEAR_MISS_LISTING", "1"),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
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

    const TIMED_OUT_LISTING_CHILD: &str =
        "installer::tests::a_listing_that_times_out_is_not_taken_as_the_raw_answer";

    /// A fake `bsdtar` whose listing hangs past the bound.
    const TIMED_OUT_LISTING_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-t" ]; then sleep 30; fi
exit 0
"#;

    #[test]
    fn a_listing_that_times_out_is_not_taken_as_the_raw_answer() {
        if std::env::var_os("PU_TEST_TIMED_OUT_LISTING").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let component = color_scheme(&root.join("installed/FakeScheme.colors"));

            let failure = unpack_download(
                &root.join("download"),
                Ok("download"),
                &component,
                "2.0",
                &root.join("work"),
            )
            .expect_err("a timed-out listing installed the download raw");

            assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
            assert_eq!(failure.message, "bsdtar timed out after 1 s");
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), TIMED_OUT_LISTING_BSDTAR);
        std::fs::write(root.join("download"), "[General]\nName=Raw\n").unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        let done = root.join("child-done");

        let output = run_child(
            TIMED_OUT_LISTING_CHILD,
            &bin,
            &[
                ("PU_TEST_TIMED_OUT_LISTING", "1"),
                ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000"),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
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

    /// A gzip cut short after its header: `bsdtar` bids gzip on it and refuses
    /// it as truncated, which is not its unrecognized-format answer.
    fn truncated_gzip(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("Scheme.colors"), "[Colors:View]\n# new\n").unwrap();
        let full = dir.join("package.tar.gz");
        let status = std::process::Command::new("tar")
            .args(["--create", "--gzip", "--file"])
            .arg(&full)
            .arg("--directory")
            .arg(&src)
            .arg("Scheme.colors")
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the gzip fixture");
        let bytes = std::fs::read(&full).unwrap();
        assert!(
            bytes.len() > 20,
            "the gzip fixture is only {} bytes",
            bytes.len()
        );
        let archive = dir.join("truncated.tar.gz");
        std::fs::write(&archive, &bytes[..20]).unwrap();
        archive
    }

    #[test]
    fn a_corrupt_archive_fails_extraction_instead_of_installing_its_bytes() {
        let root = tempfile::tempdir().unwrap();
        let archive = truncated_gzip(root.path());
        let dest = root.path().join("installed/FakeScheme.colors");
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let result = unpack_download(&archive, Ok("download"), &color_scheme(&dest), "2.0", &temp);

        let failure = result.expect_err("a corrupt archive installed");
        assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
    }

    /// Builds a gzip stream of `content` at `dir/text.gz`.
    fn gzip_of(dir: &Path, content: &str) -> PathBuf {
        let src = dir.join("text");
        std::fs::write(&src, content).unwrap();
        let archive = dir.join("text.gz");
        let out = std::fs::File::create(&archive).unwrap();
        let status = std::process::Command::new("gzip")
            .arg("-c")
            .arg(&src)
            .stdout(out)
            .status()
            .unwrap();
        assert!(status.success(), "gzip failed building the fixture");
        archive
    }

    /// A damaged archive `bsdtar` refuses to list still carries an archive
    /// signature, so it is an archive and fails the component instead of
    /// installing its bytes as the component file.
    #[test]
    fn a_truncated_ustar_tar_fails_the_extract() {
        let root = tempfile::tempdir().unwrap();
        let full = tar_holding(
            root.path(),
            "ustar",
            "Scheme.colors",
            "[Colors:View]\n# new\n",
        );
        let bytes = std::fs::read(&full).unwrap();
        let dest = root.path().join("installed/FakeScheme.colors");
        for len in [300usize, 511] {
            assert!(
                bytes.len() > len,
                "the tar fixture is only {} bytes",
                bytes.len()
            );
            let archive = root.path().join(format!("cut-{len}.tar"));
            std::fs::write(&archive, &bytes[..len]).unwrap();
            let temp = root.path().join(format!("work-{len}"));
            std::fs::create_dir_all(&temp).unwrap();

            let failure =
                unpack_download(&archive, Ok("download"), &color_scheme(&dest), "2.0", &temp)
                    .expect_err("a truncated tar installed as the component file");

            assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
        }
    }

    /// Byte 148 carries the header checksum: changing it leaves the tar's own
    /// signature in place.
    #[test]
    fn a_tar_with_a_broken_header_checksum_fails_the_extract() {
        let root = tempfile::tempdir().unwrap();
        let archive = tar_holding(
            root.path(),
            "ustar",
            "Scheme.colors",
            "[Colors:View]\n# new\n",
        );
        let mut bytes = std::fs::read(&archive).unwrap();
        bytes[148] ^= 0xff;
        std::fs::write(&archive, &bytes).unwrap();
        let dest = root.path().join("installed/FakeScheme.colors");
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let failure = unpack_download(&archive, Ok("download"), &color_scheme(&dest), "2.0", &temp)
            .expect_err("a tar with a broken header checksum installed as the component file");

        assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
    }

    /// A gzip of text holds no archive inside, so `bsdtar` refuses it as it
    /// refuses a raw file; its gzip signature still makes it an archive.
    #[test]
    fn a_gzip_of_text_fails_the_extract() {
        let root = tempfile::tempdir().unwrap();
        let archive = gzip_of(root.path(), "[General]\nName=Multi\nColorCount=2\n");
        let dest = root.path().join("installed/FakeScheme.colors");
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let failure = unpack_download(&archive, Ok("download"), &color_scheme(&dest), "2.0", &temp)
            .expect_err("a gzip of text installed as the component file");

        assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
    }

    #[test]
    fn an_empty_download_fails_the_wallpaper() {
        let root = tempfile::tempdir().unwrap();
        let download = root.path().join("download");
        std::fs::write(&download, b"").unwrap();
        let dest = root.path().join("installed/FakeWallpaper.png");
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let failure = unpack_download(&download, Ok("download"), &wallpaper(&dest), "2.0", &temp)
            .expect_err("an empty download installed as a component file");

        assert_eq!(failure.kind, FailureKind::Install, "{}", failure.message);
        assert_eq!(failure.message, "the store served an empty file");
    }

    #[test]
    fn an_empty_archive_fails_the_wallpaper_lookup() {
        let root = tempfile::tempdir().unwrap();
        let archive = empty_tar(root.path());
        let dest = root.path().join("installed/FakeWallpaper.png");
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let failure = unpack_download(&archive, Ok("download"), &wallpaper(&dest), "2.0", &temp)
            .expect_err("an empty archive installed as the component file");

        assert_eq!(failure.kind, FailureKind::Install, "{}", failure.message);
        assert_eq!(failure.message, "no wallpaper found in archive");
    }

    /// Only the single-file types name a zero-byte download as the store's
    /// empty file: every other type keeps its own lookup's line.
    #[test]
    fn an_empty_download_keeps_an_icon_theme_lookup_line() {
        let root = tempfile::tempdir().unwrap();
        let download = root.path().join("download");
        std::fs::write(&download, b"").unwrap();
        let component = InstalledComponent {
            name: "Fake Icons".to_string(),
            directory_name: "FakeIcons".to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::IconTheme,
            path: root.path().join("installed/FakeIcons"),
            is_system: false,
            release_date: String::new(),
        };
        let temp = root.path().join("work");
        std::fs::create_dir_all(&temp).unwrap();

        let failure = unpack_download(&download, Ok("download"), &component, "2.0", &temp)
            .expect_err("an empty download installed as a component file");

        assert_eq!(failure.kind, FailureKind::Install, "{}", failure.message);
        assert_eq!(failure.message, "no theme (index.theme) found in archive");
    }

    /// `bsdtar`'s unrecognized-format answer makes a download the component
    /// file only for a single-file type: every other type fails the extract.
    #[test]
    fn an_unrecognized_download_fails_the_extract_for_every_other_type() {
        for &component_type in ComponentType::all() {
            if matches!(
                component_type,
                ComponentType::ColorScheme | ComponentType::Wallpaper
            ) {
                continue;
            }
            let root = tempfile::tempdir().unwrap();
            let download = root.path().join("download");
            std::fs::write(&download, "[General]\nName=Raw\n").unwrap();
            let temp = root.path().join("work");
            std::fs::create_dir_all(&temp).unwrap();
            let component = InstalledComponent {
                name: "Fake".to_string(),
                directory_name: "Fake".to_string(),
                version: "1.0".to_string(),
                component_type,
                path: root.path().join("installed/Fake"),
                is_system: false,
                release_date: String::new(),
            };

            let failure = unpack_download(&download, Ok("download"), &component, "2.0", &temp)
                .expect_err("an unrecognized download was taken as the component itself");

            assert_eq!(
                failure.kind,
                FailureKind::Extract,
                "{component_type:?}: {}",
                failure.message
            );
            assert_eq!(
                failure.message, "bsdtar: Error opening archive: Unrecognized archive format",
                "{component_type:?}"
            );
        }
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
            let slot = InstallSlot::new();
            let outcome = verify_installed_version(
                &user_scheme_update(user_path, "2.0", 42),
                &slot.acquire(),
            );
            assert!(
                outcome.verified,
                "the user registry under XDG_DATA_HOME was not read: {:?}",
                outcome.actual_version
            );

            let system_path = PathBuf::from("/usr/share/color-schemes/FakeScheme.colors");
            let outcome = verify_installed_version(
                &system_scheme_update(system_path, "2.0", 42),
                &slot.acquire(),
            );
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
