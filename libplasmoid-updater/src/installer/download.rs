// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use super::ComponentFailure;
use crate::{Event, FailureKind, Reporter, types::InstalledComponent};

const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
const DOWNLOAD_BUFFER_SIZE: usize = 8192;

/// The directory the run unpacks downloads into.
fn temp_base() -> PathBuf {
    std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Creates a temporary directory that is automatically cleaned up on drop.
pub(crate) fn create_temp_dir() -> Result<tempfile::TempDir, ComponentFailure> {
    let base = temp_base();
    tempfile::Builder::new()
        .prefix("plasmoid-updater-")
        .tempdir_in(&base)
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Install, format!("{}: {e}", base.display()))
        })
}

/// Removes the download directories a killed run left in the temp dir.
///
/// Only a caller holding the run lock may call this: a directory with this
/// prefix is then necessarily dead, while a live run's own temp dir would be
/// exactly the same shape. Only directories this user owns are removed, so a
/// `--system` run under root leaves another user's directories alone: those
/// belong to a run whose lock this process does not hold.
pub(crate) fn remove_stale_temp_dirs() {
    sweep_stale_dirs(&temp_base(), nix::unistd::Uid::effective().as_raw());
}

fn sweep_stale_dirs(base: &Path, owner: u32) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let is_download_dir = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("plasmoid-updater-"));
        let owned = entry
            .metadata()
            .is_ok_and(|metadata| metadata.uid() == owner);
        if is_download_dir
            && owned
            && let Err(e) = remove_tree(&path)
        {
            log::warn!(target: "cleanup", "failed to remove stale temp dir {}: {e}", path.display());
        }
    }
}

/// Removes a directory tree whose own modes may forbid deleting its entries.
///
/// Unlinking an entry needs write permission on the directory holding it, and
/// an archive keeps the modes it shipped, so a download extracted with a
/// read-only directory inside would otherwise outlive every sweep.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    grant_owner_access(path);
    fs::remove_dir_all(path)
}

/// Gives its owner write access to every directory below `path`, which is what
/// removing their entries takes.
fn grant_owner_access(path: &Path) {
    // A link is removed as its parent's entry and never walked: its target is
    // not this run's to change.
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.is_dir() {
        return;
    }
    let mode = metadata.mode() & 0o7777;
    if mode & 0o700 != 0o700
        && let Err(e) = fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))
    {
        log::debug!(target: "cleanup", "failed to make {} removable: {e}", path.display());
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        grant_owner_access(&entry.path());
    }
}

/// Downloads a package with optional checksum verification, emitting
/// [`Event::Downloading`] through `reporter` as chunks arrive: the event's
/// `name` is the component's display name and its `id` the installed path.
///
/// The component's directory name namespaces the download file, preventing
/// filename collisions when multiple components download in parallel.
pub(crate) fn download_package(
    client: &reqwest::blocking::Client,
    url: &str,
    expected_checksum: Option<&str>,
    component: &InstalledComponent,
    temp_path: &Path,
    reporter: &dyn Reporter,
) -> Result<PathBuf, ComponentFailure> {
    let file_name = url.rsplit('/').next().unwrap_or("package.tar.gz");

    let dest = temp_path.join(format!("{}_{file_name}", component.directory_name));

    let response = client
        .get(url)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .send()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Download, format!("request failed: {e}"))
        })?;

    if !response.status().is_success() {
        return Err(ComponentFailure::new(
            FailureKind::Download,
            format!("http status {}", response.status()),
        ));
    }

    let total = response.content_length();

    let mut file =
        File::create(&dest).map_err(|e| ComponentFailure::io(FailureKind::Download, &dest, e))?;
    let mut hasher = md5::Context::new();

    let mut reader = response;
    let mut buffer = [0u8; DOWNLOAD_BUFFER_SIZE];
    let mut received: u64 = 0;
    let id = component.event_id();

    loop {
        let bytes_read = reader.read(&mut buffer).map_err(|e| {
            ComponentFailure::new(FailureKind::Download, format!("read error: {e}"))
        })?;

        if bytes_read == 0 {
            break;
        }

        let chunk = &buffer[..bytes_read];
        hasher.consume(chunk);
        file.write_all(chunk)
            .map_err(|e| ComponentFailure::io(FailureKind::Download, &dest, e))?;

        received += bytes_read as u64;
        reporter.event(&Event::Downloading {
            name: component.name.clone(),
            id: id.clone(),
            bytes: received,
            total,
        });
    }

    // verify checksum if provided
    if let Some(expected) = expected_checksum {
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected.to_lowercase() {
            fs::remove_file(&dest).ok();
            return Err(ComponentFailure::new(
                FailureKind::Checksum,
                format!("checksum mismatch: expected {expected}, got {actual}"),
            ));
        }
        log::debug!(target: "checksum", "verified md5 for {file_name}");
    }

    Ok(dest)
}

/// Extracts a package archive to the destination directory using `bsdtar`.
pub(crate) fn extract_archive(archive_path: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    fs::create_dir_all(dest).map_err(|e| ComponentFailure::io(FailureKind::Extract, dest, e))?;

    let output = Command::new("bsdtar")
        .args([
            "-xf",
            &archive_path.to_string_lossy(),
            "-C",
            &dest.to_string_lossy(),
        ])
        .output()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Extract, format!("failed to run bsdtar: {e}"))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if stderr.trim().is_empty() {
            format!("bsdtar exited with status {}", output.status)
        } else {
            stderr.trim().to_string()
        };
        return Err(ComponentFailure::new(FailureKind::Extract, detail));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_temp_dir_is_raii() {
        let temp = create_temp_dir().unwrap();
        let path = temp.path().to_path_buf();
        assert!(path.exists());
        std::fs::write(path.join("test.txt"), b"data").unwrap();
        drop(temp);
        assert!(!path.exists());
    }

    #[test]
    fn sweep_removes_stale_download_dirs_only() {
        let base = tempfile::tempdir().unwrap();
        let stale = base.path().join("plasmoid-updater-Xk3q9z");
        std::fs::create_dir_all(stale.join("extracted")).unwrap();
        std::fs::write(stale.join("archive.tar.gz"), b"partial").unwrap();
        let unrelated = base.path().join("something-else");
        std::fs::create_dir_all(&unrelated).unwrap();

        sweep_stale_dirs(base.path(), nix::unistd::Uid::effective().as_raw());

        assert!(!stale.exists(), "the stale download dir survived the sweep");
        assert!(unrelated.is_dir());
    }

    #[test]
    fn sweep_removes_a_stale_dir_holding_a_read_only_subdirectory() {
        use std::os::unix::fs::PermissionsExt;

        if crate::installer::privilege::is_root() {
            return; // root unlinks out of a 0o555 directory too
        }
        let base = tempfile::tempdir().unwrap();
        let stale = base.path().join("plasmoid-updater-Xk3q9z");
        let extracted = stale.join("extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        std::fs::write(extracted.join("package.txt"), b"partial").unwrap();
        // An archive's own modes can leave the extracted directory read-only.
        std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o555)).unwrap();

        sweep_stale_dirs(base.path(), nix::unistd::Uid::effective().as_raw());

        assert!(
            !stale.exists(),
            "a read-only subdirectory kept the stale dir alive"
        );
    }

    #[test]
    fn sweep_leaves_the_fallback_home_alone() {
        let base = tempfile::tempdir().unwrap();
        let name = crate::paths::fallback_home()
            .file_name()
            .expect("the fallback home has a file name")
            .to_owned();
        let fallback = base.path().join(name);
        std::fs::create_dir_all(fallback.join(".cache")).unwrap();
        std::fs::write(fallback.join(".cache/notes.txt"), b"kept").unwrap();
        let stale = base.path().join("plasmoid-updater-Xk3q9z");
        std::fs::create_dir_all(&stale).unwrap();

        sweep_stale_dirs(base.path(), nix::unistd::Uid::effective().as_raw());

        assert!(
            fallback.join(".cache/notes.txt").exists(),
            "the sweep removed the fallback home"
        );
        assert!(!stale.exists(), "control: a stale download dir must go");
    }

    #[test]
    fn sweep_leaves_a_directory_another_user_owns() {
        let base = tempfile::tempdir().unwrap();
        // What a root `--system` run sees: another user's live download dir.
        let stale = base.path().join("plasmoid-updater-1001");
        std::fs::create_dir_all(stale.join("extracted")).unwrap();
        let effective = nix::unistd::Uid::effective().as_raw();

        sweep_stale_dirs(base.path(), effective.wrapping_add(1));

        assert!(stale.is_dir(), "a directory another user owns was removed");

        // Control: the same fixture goes when it carries this process's uid, so
        // the assertion above is about the owner and not an unsweepable fixture.
        sweep_stale_dirs(base.path(), effective);
        assert!(!stale.exists(), "the fixture itself is not sweepable");
    }
}
