// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use super::ComponentFailure;
use crate::{Event, FailureKind, Reporter, types::InstalledComponent};

const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
const DOWNLOAD_BUFFER_SIZE: usize = 8192;

/// Creates a temporary directory that is automatically cleaned up on drop.
pub(crate) fn create_temp_dir() -> Result<tempfile::TempDir, ComponentFailure> {
    let base = std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    tempfile::Builder::new()
        .prefix("plasmoid-updater-")
        .tempdir_in(&base)
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Install, format!("{}: {e}", base.display()))
        })
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
}
