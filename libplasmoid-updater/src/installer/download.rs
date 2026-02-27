// SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use crate::{Error, Result};

const DOWNLOAD_TIMEOUT_SECS: u64 = 10;
const DOWNLOAD_BUFFER_SIZE: usize = 8192;

pub(crate) fn temp_dir() -> PathBuf {
    std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("plasmoid-updater")
}

/// Downloads a package with optional checksum verification.
pub(crate) fn download_package(
    client: &reqwest::blocking::Client,
    url: &str,
    expected_checksum: Option<&str>,
) -> Result<PathBuf> {
    let temp = temp_dir();
    fs::create_dir_all(&temp)?;

    let file_name = url
        .rsplit('/')
        .next()
        .unwrap_or("package.tar.gz")
        .to_string();

    let dest = temp.join(&file_name);

    let response = client
        .get(url)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .send()
        .map_err(|e| Error::download(format!("request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(Error::download(format!(
            "http status {}",
            response.status()
        )));
    }

    let mut file = File::create(&dest)?;
    let mut hasher = md5::Context::new();

    let mut reader = response;
    let mut buffer = [0u8; DOWNLOAD_BUFFER_SIZE];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|e| Error::download(format!("read error: {e}")))?;

        if bytes_read == 0 {
            break;
        }

        let chunk = &buffer[..bytes_read];
        hasher.consume(chunk);
        file.write_all(chunk)?;
    }

    // verify checksum if provided
    if let Some(expected) = expected_checksum {
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected.to_lowercase() {
            fs::remove_file(&dest).ok();
            return Err(Error::checksum(expected, actual));
        }
        log::debug!(target: "checksum", "verified md5 for {file_name}");
    }

    Ok(dest)
}

/// Extracts a package archive to the destination directory using `bsdtar`.
pub(crate) fn extract_archive(archive_path: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;

    let status = Command::new("bsdtar")
        .args([
            "-xf",
            &archive_path.to_string_lossy(),
            "-C",
            &dest.to_string_lossy(),
        ])
        .status()
        .map_err(|e| Error::extraction(format!("failed to run bsdtar: {e}")))?;

    if !status.success() {
        return Err(Error::extraction(format!(
            "bsdtar exited with status {}",
            status
        )));
    }

    Ok(())
}
