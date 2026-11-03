// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    fs::{self, File},
    io::{BufRead as _, Read, Write},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use super::ComponentFailure;
use crate::{AvailableUpdate, Event, FailureKind, Reporter};

const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
const DOWNLOAD_BUFFER_SIZE: usize = 8192;
/// The cap on an unverified download whose entry advertises no size at all.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// The most an archive may claim to unpack to before extraction is refused.
const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The most members an archive may hold before extraction is refused.
const MAX_EXTRACTED_MEMBERS: u64 = 100_000;
/// How far past twice its advertised size an unverified download may run: the
/// store under-reports often enough (3 of 21 measurable no-MD5 entries served
/// more than advertised plus a KiB) that the limit carries a margin.
const ADVERTISED_SIZE_SLACK_BYTES: u64 = 1024 * 1024;

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

/// The bytes a download may write before it is refused.
///
/// A verified download is bounded by the extraction cap: nothing past it could
/// be unpacked. An unverified one is bounded by twice the size its entry
/// advertised plus a margin for the store's under-reporting, and falls back to
/// [`MAX_DOWNLOAD_BYTES`] when no size is advertised either.
fn download_limit(checksum: Option<&str>, advertised: Option<u64>) -> u64 {
    match (checksum, advertised) {
        (Some(_), _) => MAX_EXTRACTED_BYTES,
        (None, Some(size)) => size
            .saturating_mul(2)
            .saturating_add(ADVERTISED_SIZE_SLACK_BYTES),
        (None, None) => MAX_DOWNLOAD_BYTES,
    }
}

/// Downloads a package with optional checksum verification, emitting
/// [`Event::Downloading`] through `reporter` as chunks arrive: the event's
/// `name` is the component's display name and its `id` the installed path.
///
/// A download without an advertised MD5 is logged as unverified and capped at
/// twice its advertised size plus a margin, or at [`MAX_DOWNLOAD_BYTES`] when
/// no size is given; a verified one is capped at [`MAX_EXTRACTED_BYTES`].
/// Reading past the cap is a download failure.
///
/// The component's directory name namespaces the download file, preventing
/// filename collisions when multiple components download in parallel.
pub(crate) fn download_package(
    update: &AvailableUpdate,
    client: &reqwest::blocking::Client,
    temp_path: &Path,
    reporter: &dyn Reporter,
) -> Result<PathBuf, ComponentFailure> {
    let component = &update.installed;
    let url = &update.download_url;
    let file_name = url.rsplit('/').next().unwrap_or("package.tar.gz");

    let dest = temp_path.join(format!("{}_{file_name}", component.directory_name));
    let limit = download_limit(update.checksum.as_deref(), update.download_size);

    if update.checksum.is_none() {
        log::warn!(
            target: "download",
            "{}: the store advertises no checksum, so the download is unverified and capped at {limit} bytes",
            component.name,
        );
    }

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
    if let Some(length) = total
        && length > limit
    {
        return Err(ComponentFailure::new(
            FailureKind::Download,
            format!("the download advertises {length} bytes, over the {limit}-byte limit"),
        ));
    }

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
        if received > limit {
            fs::remove_file(&dest).ok();
            return Err(ComponentFailure::new(
                FailureKind::Download,
                format!("the download exceeded the {limit}-byte limit"),
            ));
        }
        reporter.event(&Event::Downloading {
            name: component.name.clone(),
            id: id.clone(),
            bytes: received,
            total,
        });
    }

    // verify checksum if provided
    if let Some(expected) = update.checksum.as_deref() {
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

/// Caps an extraction listing as its lines arrive.
struct ListingBudget {
    bytes: u64,
    members: u64,
}

impl ListingBudget {
    fn new() -> Self {
        Self {
            bytes: 0,
            members: 0,
        }
    }

    /// Counts one listed member, refusing as soon as either cap is crossed.
    fn add(&mut self, line: &str) -> Result<(), ComponentFailure> {
        self.members += 1;
        if let Some(size) = member_size(line) {
            self.bytes = self.bytes.saturating_add(size);
        }
        if self.bytes > MAX_EXTRACTED_BYTES {
            return Err(ComponentFailure::new(
                FailureKind::Extract,
                format!("the archive unpacks to over the {MAX_EXTRACTED_BYTES}-byte cap"),
            ));
        }
        if self.members > MAX_EXTRACTED_MEMBERS {
            return Err(ComponentFailure::new(
                FailureKind::Extract,
                format!("the archive holds more than the {MAX_EXTRACTED_MEMBERS}-member cap"),
            ));
        }
        Ok(())
    }
}

/// The size column of one `bsdtar -tvf` line.
fn member_size(line: &str) -> Option<u64> {
    line.split_whitespace().nth(4)?.parse().ok()
}

/// Extracts a package archive to the destination directory using `bsdtar`.
///
/// The archive is listed first, one line at a time: as soon as the members add
/// up past [`MAX_EXTRACTED_BYTES`] or the archive holds more than
/// [`MAX_EXTRACTED_MEMBERS`], the listing child is killed and nothing is
/// written, so a hostile archive cannot fill the disk or hold its whole
/// listing in memory.
pub(crate) fn extract_archive(archive_path: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    let mut child = Command::new("bsdtar")
        // Numeric owners keep the size in the same column whatever user and
        // group names the archive itself carries.
        .args(["-t", "-v", "--numeric-owner", "-f"])
        .arg(archive_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Extract, format!("failed to run bsdtar: {e}"))
        })?;

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ComponentFailure::new(
            FailureKind::Extract,
            "bsdtar produced no listing stream".to_string(),
        ));
    };
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    let mut budget = ListingBudget::new();
    let mut listing = std::io::BufReader::new(stdout);
    let mut line = Vec::new();
    let refusal = loop {
        line.clear();
        match listing.read_until(b'\n', &mut line) {
            Ok(0) => break None,
            Ok(_) => {
                if let Err(e) = budget.add(&String::from_utf8_lossy(&line)) {
                    break Some(e);
                }
            }
            Err(e) => {
                break Some(ComponentFailure::new(
                    FailureKind::Extract,
                    format!("failed to read the archive listing: {e}"),
                ));
            }
        }
    };

    if let Some(failure) = refusal {
        let _ = child.kill();
        let _ = child.wait();
        if let Some(reader) = stderr_reader {
            let _ = reader.join();
        }
        return Err(failure);
    }

    let status = child.wait().map_err(|e| {
        ComponentFailure::new(
            FailureKind::Extract,
            format!("failed to wait for bsdtar: {e}"),
        )
    })?;
    let stderr = stderr_reader.map_or_else(Vec::new, |reader| reader.join().unwrap_or_default());
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let detail = if stderr.trim().is_empty() {
            crate::error::command_failure("bsdtar", &status)
        } else {
            stderr.trim().to_string()
        };
        return Err(ComponentFailure::new(FailureKind::Extract, detail));
    }

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
            crate::error::command_failure("bsdtar", &output.status)
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
    use crate::types::{ComponentType, InstalledComponent};

    fn update_fixture(url: &str, checksum: Option<&str>, size: Option<u64>) -> AvailableUpdate {
        AvailableUpdate {
            installed: InstalledComponent {
                name: "Fake Scheme".to_string(),
                directory_name: "FakeScheme.colors".to_string(),
                version: "1.0".to_string(),
                component_type: ComponentType::ColorScheme,
                path: PathBuf::from("/nonexistent/FakeScheme.colors"),
                is_system: false,
                release_date: String::new(),
            },
            content_id: 1,
            latest_version: "2.0".to_string(),
            download_url: url.to_string(),
            store_url: String::new(),
            release_date: String::new(),
            checksum: checksum.map(str::to_string),
            download_size: size,
        }
    }

    /// Serves `body` once over HTTP on a loopback port and returns its URL.
    fn serve_once(body: Vec<u8>) -> String {
        serve_with_length(body, true)
    }

    /// Serves `body` once, declaring its `Content-Length` or sending it
    /// close-delimited (no length header) as a chunked or streaming store would.
    fn serve_with_length(body: Vec<u8>, declare_length: bool) -> String {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let head = if declare_length {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
            } else {
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_string()
            };
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        });
        format!("http://127.0.0.1:{port}/package.tar.gz")
    }

    #[test]
    fn an_unverified_download_is_capped_at_its_advertised_size_and_warns() {
        use crate::testutil::logs::{capture_logs, captured_logs};

        capture_logs();
        let client = reqwest::blocking::Client::new();
        let dir = tempfile::tempdir().unwrap();

        // Control: the same body under a limit that covers it is accepted, so
        // the failure below is the limit and not the fixture.
        let accepted_url = serve_once(vec![0u8; 4096]);
        let accepted = download_package(
            &update_fixture(&accepted_url, None, Some(4096)),
            &client,
            dir.path(),
            &(),
        );
        assert!(accepted.is_ok(), "{accepted:?}");

        let capped_url = serve_once(vec![0u8; 2 * 1024 * 1024]);
        let refusal = download_package(
            &update_fixture(&capped_url, None, Some(1024)),
            &client,
            dir.path(),
            &(),
        )
        .expect_err("an unverified download ran past its advertised size");
        assert_eq!(refusal.kind, FailureKind::Download);
        assert!(refusal.message.contains("limit"), "{}", refusal.message);

        let lines = captured_logs();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("download: ") && line.contains("unverified")),
            "an unverified download was not reported: {lines:?}"
        );
    }

    #[test]
    fn an_archive_unpacking_past_the_cap_is_refused() {
        let mut budget = ListingBudget::new();
        let refusal = budget
            .add("-rw-r--r--  0 0 0 3000000000 Nov  2 10:00 big")
            .unwrap_err();
        assert_eq!(refusal.kind, FailureKind::Extract);
        assert!(refusal.message.contains("cap"), "{}", refusal.message);

        // Control: a listing under the cap passes.
        let mut control = ListingBudget::new();
        control
            .add("-rw-r--r--  0 0 0 1024 Nov  2 10:00 small")
            .unwrap();
    }

    /// The streaming branch bounds a body that advertises no length: the
    /// `Content-Length` refusal never runs for it.
    #[test]
    fn a_download_past_its_limit_without_a_length_header_is_refused() {
        let client = reqwest::blocking::Client::new();
        let dir = tempfile::tempdir().unwrap();

        // Control: a close-delimited body under the limit is accepted, so the
        // refusal below is the limit and not the missing header.
        let accepted_url = serve_with_length(vec![0u8; 1024], false);
        let accepted = download_package(
            &update_fixture(&accepted_url, None, Some(4096)),
            &client,
            dir.path(),
            &(),
        );
        assert!(accepted.is_ok(), "{accepted:?}");

        let capped_url = serve_with_length(vec![0u8; 2 * 1024 * 1024], false);
        let refusal = download_package(
            &update_fixture(&capped_url, None, Some(1024)),
            &client,
            dir.path(),
            &(),
        )
        .expect_err("a close-delimited download ran past its advertised size");
        assert_eq!(refusal.kind, FailureKind::Download);
    }

    #[test]
    fn an_archive_with_more_members_than_the_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("f"), b"").unwrap();
        let names = dir.path().join("names");
        std::fs::write(&names, "f\n".repeat(MAX_EXTRACTED_MEMBERS as usize + 1)).unwrap();
        let archive = dir.path().join("many.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("--create")
            .arg("--gzip")
            .arg("--file")
            .arg(&archive)
            .arg("--directory")
            .arg(&src)
            .arg("--files-from")
            .arg(&names)
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the fixture archive");

        let dest = dir.path().join("out");
        let refusal = extract_archive(&archive, &dest).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Extract);
        assert!(refusal.message.contains("cap"), "{}", refusal.message);
        assert!(
            !dest.join("f").exists(),
            "the over-cap archive was extracted"
        );
    }

    /// A sparse member declares a size the archive does not hold, so the cap
    /// has to be decided from the real listing rather than from the file size.
    #[test]
    fn an_archive_claiming_more_than_the_cap_is_refused_through_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        std::fs::File::create(src.join("big"))
            .unwrap()
            .set_len(3 * 1024 * 1024 * 1024)
            .unwrap();
        let archive = dir.path().join("sparse.tar");
        let status = std::process::Command::new("tar")
            .args(["--create", "--sparse", "--file"])
            .arg(&archive)
            .arg("--directory")
            .arg(&src)
            .arg("big")
            .status()
            .unwrap();
        assert!(status.success(), "tar failed building the sparse fixture");
        let stored = std::fs::metadata(&archive).unwrap().len();
        assert!(
            stored < 1024 * 1024,
            "the sparse fixture holds {stored} bytes, so it is not sparse"
        );

        let dest = dir.path().join("out");
        let refusal = extract_archive(&archive, &dest).unwrap_err();

        assert_eq!(refusal.kind, FailureKind::Extract);
        assert!(refusal.message.contains("cap"), "{}", refusal.message);
        assert!(!dest.exists(), "an over-cap archive was extracted");
    }

    #[test]
    fn a_verified_download_is_capped_at_the_extraction_cap() {
        assert_eq!(
            download_limit(Some("d41d8cd98f00b204e9800998ecf8427e"), Some(4096)),
            MAX_EXTRACTED_BYTES
        );
        assert_eq!(
            download_limit(Some("d41d8cd98f00b204e9800998ecf8427e"), None),
            MAX_EXTRACTED_BYTES
        );
    }

    #[test]
    fn an_unverified_download_is_capped_at_twice_its_advertised_size_plus_a_mib() {
        assert_eq!(download_limit(None, Some(4096)), 2 * 4096 + 1024 * 1024);
        assert_eq!(download_limit(None, None), MAX_DOWNLOAD_BYTES);
    }

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

    /// A bsdtar that dies with empty stderr names its exit status the way every
    /// other command does, not `bsdtar exited with status exit status: 1`.
    #[test]
    fn a_bsdtar_failure_with_empty_stderr_names_the_exit_status() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::PathBuf;

        if std::env::var_os("PU_TEST_BSDTAR_CHILD").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_BSDTAR_ROOT").unwrap());
            let archive = root.join("package.tar.gz");
            std::fs::write(&archive, b"not an archive").unwrap();
            let dest = root.join("out");

            let refusal = extract_archive(&archive, &dest).unwrap_err();

            assert_eq!(refusal.kind, FailureKind::Extract);
            assert_eq!(refusal.message, "bsdtar exited with status 1");
            std::fs::write(root.join("done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake_bsdtar = bin.join("bsdtar");
        std::fs::write(
            &fake_bsdtar,
            "#!/bin/sh\ncase \"$1\" in -t) exit 0;; *) exit 1;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_bsdtar, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = std::ffi::OsString::from(format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ));
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::download::tests::a_bsdtar_failure_with_empty_stderr_names_the_exit_status",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", path)
            .env("PU_TEST_BSDTAR_CHILD", "1")
            .env("PU_TEST_BSDTAR_ROOT", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
