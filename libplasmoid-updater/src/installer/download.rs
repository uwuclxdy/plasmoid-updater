// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    fs::{self, File},
    io::{BufRead as _, Read, Write},
    os::unix::{
        fs::{MetadataExt as _, PermissionsExt as _},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};

use super::{
    ComponentFailure,
    install::{
        KILL_GRACE, REAP_BOUND, Route, join_readers_bounded, reap_within, run_with_timeout,
        spawn_reader,
    },
};
use crate::{AvailableUpdate, Event, FailureKind, Reporter};

const DOWNLOAD_TIMEOUT_SECS: u64 = 60;
const DOWNLOAD_BUFFER_SIZE: usize = 8192;
/// The cap on an unverified download whose entry advertises no known size: none
/// at all, or an advertised 0 counting as no size.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
/// The most an archive may claim to unpack to before extraction is refused,
/// and the ceiling on the limit for any download.
const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The most members an archive may hold before extraction is refused.
const MAX_EXTRACTED_MEMBERS: u64 = 100_000;
/// How far past twice its advertised size an unverified download may run, below
/// the [`MAX_EXTRACTED_BYTES`] ceiling: the store under-reports often enough (3
/// of 21 measurable no-MD5 entries served more than advertised plus a KiB) that
/// the limit carries a margin.
const ADVERTISED_SIZE_SLACK_BYTES: u64 = 1024 * 1024;
/// How long a `bsdtar` call (the listing or the extraction) may run before its
/// process group is killed. Extraction cost follows member count and directory
/// depth rather than archive size: a 99,000-member, 101 MB archive extracted in
/// 15-20 s, while a 12.8 MB archive of 3,000 nested directories took 65-77 s
/// (measured 2026-11-23 on an NVMe disk under heavy load). 120 s leaves the
/// measured slow shape room to finish; a slower disk or an archive of the
/// slowest per-member shape near the member cap can still overrun it. A hung
/// `bsdtar` costs the run the bound instead of the update lock.
const BSDTAR_TIMEOUT: Duration = Duration::from_secs(120);

fn bsdtar_timeout() -> Duration {
    // A unit test cannot wait out the production deadline, so it may shorten it.
    #[cfg(test)]
    {
        if let Some(millis) = std::env::var("PU_TEST_BSDTAR_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            return Duration::from_millis(millis);
        }
    }
    BSDTAR_TIMEOUT
}

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
/// advertised plus a margin for the store's under-reporting, never above
/// [`MAX_EXTRACTED_BYTES`], and falls back to [`MAX_DOWNLOAD_BYTES`] when no
/// size is advertised either, an advertised 0 counting as no size.
fn download_limit(checksum: Option<&str>, advertised: Option<u64>) -> u64 {
    match (checksum, advertised) {
        (Some(_), _) => MAX_EXTRACTED_BYTES,
        (None, Some(size)) if size > 0 => size
            .saturating_mul(2)
            .saturating_add(ADVERTISED_SIZE_SLACK_BYTES)
            .min(MAX_EXTRACTED_BYTES),
        (None, _) => MAX_DOWNLOAD_BYTES,
    }
}

/// The file name the store serves a download under: the last path segment of
/// its URL, its `?query` and `#fragment` dropped and its `%xx` escapes
/// decoded; a URL naming no path segment names nothing. `Err` carries the
/// segment as the store spelled it when its escapes do not decode to UTF-8: a
/// name the install refuses wherever it would put it in the tree. The name
/// need not be a file name; the install step refuses it where it would name
/// the component's own file.
pub(super) fn served_file_name(url: &str) -> Result<String, String> {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let path = match url[..end].split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("", |slash| &rest[slash..]),
        None => &url[..end],
    };
    decode_percent(path.rsplit_once('/').map_or(path, |(_, name)| name))
}

/// The text after the last '/' in `url`, query included, as the store spelled
/// it, for a temp file whose name nothing else reads.
fn raw_served_name(url: &str) -> &str {
    url.rsplit_once('/').map_or(url, |(_, name)| name)
}

/// `name` with every `%xx` escape decoded; an escape that is not two hex
/// digits stays as it is. `Err` is `name` itself when the decoded bytes are
/// not UTF-8.
fn decode_percent(name: &str) -> Result<String, String> {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = name.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = if bytes[index] == b'%' && index + 2 < bytes.len() {
            hex(bytes[index + 1])
                .zip(hex(bytes[index + 2]))
                .map(|(high, low)| high * 16 + low)
        } else {
            None
        };
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| name.to_string())
}

/// Downloads a package with optional checksum verification, emitting
/// [`Event::Downloading`] through `reporter` as chunks arrive: the event's
/// `name` is the component's display name and its `id` the installed path.
///
/// A download without an advertised MD5 is logged as unverified and capped at
/// twice its advertised size plus a margin, never above [`MAX_EXTRACTED_BYTES`],
/// or at [`MAX_DOWNLOAD_BYTES`] when no size is given, an advertised 0 counting
/// as none; a verified one is capped at [`MAX_EXTRACTED_BYTES`]. Reading past
/// the cap is a download failure.
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
    // The temp file only needs a name no parallel download shares; the
    // component's own name is decoded where it is used.
    let file_name = raw_served_name(url);

    let dest = temp_path.join(format!("{}_{file_name}", component.directory_name));
    let limit = download_limit(update.checksum.as_deref(), update.download_size);

    if update.checksum.is_none() {
        log::warn!(
            target: "download",
            "{:?}: the store advertises no checksum, so the download is unverified and capped at {limit} bytes",
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

/// Ends a listing that is not to finish on its own: kills the whole process
/// group, reaps the child within [`REAP_BOUND`] and drains its stderr within
/// [`KILL_GRACE`]; a reader still holding the pipe when the grace runs out is
/// left detached.
fn end_listing(
    child: &mut std::process::Child,
    group: Pid,
    stderr_reader: Option<std::thread::JoinHandle<Vec<u8>>>,
) {
    let _ = killpg(group, Signal::SIGKILL);
    drop(reap_within(child, REAP_BOUND));
    if let Some(reader) = stderr_reader {
        drop(join_readers_bounded(
            vec![reader],
            Instant::now() + KILL_GRACE,
        ));
    }
}

/// The listing's failure when the deadline killed it.
fn listing_timeout(timeout: Duration) -> ComponentFailure {
    ComponentFailure::new(
        FailureKind::Extract,
        format!("bsdtar timed out after {} s", timeout.as_secs()),
    )
}

/// The exact refusal `bsdtar -t` prints for a file libarchive bids no archive
/// format on. A single-file component's download can be the component file
/// itself only when its listing fails with exactly this answer; every other
/// listing failure is an extract failure.
const UNRECOGNIZED_FORMAT: &str = "bsdtar: Error opening archive: Unrecognized archive format";

/// Lists `archive_path`, capping what its lines add up to.
///
/// The tool runs in a process group of its own, and the whole group is killed
/// at `timeout`. The listing's pipe can still stay open past that kill: a
/// `bsdtar` hanging before its first line holds its own write end, and a
/// descendant that left the group holds a copy of it. So the lines are read on
/// a thread of their own, and the caller waits for each only until the
/// deadline.
///
/// A file `bsdtar` bids no archive format on is refused with
/// [`ComponentFailure::unrecognized_format`] set, so a caller can tell that
/// answer from every other listing failure.
fn list_archive(archive_path: &Path, timeout: Duration) -> Result<(), ComponentFailure> {
    let mut budget = ListingBudget::new();
    let mut child = Command::new("bsdtar")
        // Numeric owners keep the size in the same column whatever user and
        // group names the archive itself carries.
        .args(["-t", "-v", "--numeric-owner", "-f"])
        .arg(archive_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| {
            ComponentFailure::new(FailureKind::Extract, format!("failed to run bsdtar: {e}"))
        })?;
    let group = Pid::from_raw(child.id().cast_signed());

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ComponentFailure::new(
            FailureKind::Extract,
            "bsdtar produced no listing stream",
        ));
    };
    let stderr_reader = child.stderr.take().map(spawn_reader);

    // A listing line carries an untrusted member path, and a pax path of 1 MiB
    // lists as one line, so the reader may hold only three lines (two queued,
    // one waiting to be sent) beyond the one the budget is judging.
    let (sender, lines) = mpsc::sync_channel(2);
    std::thread::spawn(move || {
        let mut listing = std::io::BufReader::new(stdout);
        loop {
            let mut line = Vec::new();
            let sent = match listing.read_until(b'\n', &mut line) {
                Ok(0) => return,
                Ok(_) => sender.send(Ok(line)),
                Err(e) => sender.send(Err(e)),
            };
            if sent.is_err() {
                return;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let refusal = loop {
        match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Ok(line)) => {
                if let Err(e) = budget.add(&String::from_utf8_lossy(&line)) {
                    break Some(e);
                }
            }
            Ok(Err(e)) => {
                break Some(ComponentFailure::new(
                    FailureKind::Extract,
                    format!("failed to read the archive listing: {e}"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                end_listing(&mut child, group, stderr_reader);
                return Err(listing_timeout(timeout));
            }
        }
    };
    drop(lines);

    if let Some(failure) = refusal {
        end_listing(&mut child, group, stderr_reader);
        return Err(failure);
    }

    // A listing can end its stdout and still hold on, so the child keeps the
    // same deadline: one that outlives it is a hung call like any other.
    let reaped = match reap_within(
        &mut child,
        deadline.saturating_duration_since(Instant::now()),
    ) {
        Ok(reaped) => reaped,
        Err(e) => {
            end_listing(&mut child, group, stderr_reader);
            return Err(ComponentFailure::new(
                FailureKind::Extract,
                format!("failed to wait for bsdtar: {e}"),
            ));
        }
    };
    if !reaped {
        end_listing(&mut child, group, stderr_reader);
        return Err(listing_timeout(timeout));
    }
    let status = child.wait().map_err(|e| {
        ComponentFailure::new(
            FailureKind::Extract,
            format!("failed to wait for bsdtar: {e}"),
        )
    })?;
    // A descendant of a tool that already exited can still hold the pipe open,
    // so the drain is bounded like the readers of a killed group.
    let stderr = stderr_reader.map_or_else(Vec::new, |reader| {
        let (mut drained, _) = join_readers_bounded(vec![reader], Instant::now() + KILL_GRACE);
        drained.pop().unwrap_or_default()
    });
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        let detail = stderr.trim();
        let mut failure = ComponentFailure::new(
            FailureKind::Extract,
            if detail.is_empty() {
                crate::error::command_failure("bsdtar", &status)
            } else {
                detail.to_string()
            },
        );
        failure.unrecognized_format = detail == UNRECOGNIZED_FORMAT;
        return Err(failure);
    }
    Ok(())
}

/// The extract stage's kind for a failure the shared timeout helper reports as
/// an install failure.
fn extraction_failure(failure: ComponentFailure) -> ComponentFailure {
    let mut extract = ComponentFailure::new(FailureKind::Extract, failure.message);
    extract.process_may_still_run = failure.process_may_still_run;
    extract
}

/// Extracts a package archive to the destination directory using `bsdtar`.
///
/// The archive is listed first, one line at a time: as soon as the members add
/// up past [`MAX_EXTRACTED_BYTES`] or the archive holds more than
/// [`MAX_EXTRACTED_MEMBERS`], the listing child is killed and nothing is
/// written, so a hostile archive cannot fill the disk or hold its whole
/// listing in memory.
///
/// Both `bsdtar` calls run under [`BSDTAR_TIMEOUT`], each in a process group of
/// its own, so a hung tool fails the component as an extract failure naming the
/// bound instead of holding the update lock, and the call waits on its pipes no
/// longer than the kill grace.
pub(crate) fn extract_archive(archive_path: &Path, dest: &Path) -> Result<(), ComponentFailure> {
    let timeout = bsdtar_timeout();
    list_archive(archive_path, timeout)?;

    fs::create_dir_all(dest).map_err(|e| ComponentFailure::io(FailureKind::Extract, dest, e))?;

    let mut command = Command::new("bsdtar");
    command.args([
        "-xf",
        &archive_path.to_string_lossy(),
        "-C",
        &dest.to_string_lossy(),
    ]);
    let output = run_with_timeout(&mut command, Route::Direct, timeout, "bsdtar")
        .map_err(extraction_failure)?;

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
    use super::super::install::tests::{
        child_command, process_running, run_child, stop_recorded, write_script,
    };
    use super::*;
    use crate::types::{ComponentType, InstalledComponent};

    /// The host's `bsdtar`, which a sandbox's own `bsdtar` wraps.
    fn real_bsdtar() -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("bsdtar"))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("no bsdtar on PATH"))
    }

    /// Whether the pid a fake tool recorded at `path` still runs.
    fn recorded_still_running(path: &Path) -> bool {
        let pid: i32 = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("no pid recorded at {}: {e}", path.display()))
            .trim()
            .parse()
            .unwrap();
        process_running(pid)
    }

    /// A timeout of `seconds`, the grace a killed process group gets, and slack
    /// for a loaded runner.
    fn within_bound(seconds: u64) -> std::time::Duration {
        std::time::Duration::from_secs(seconds)
            + super::super::install::KILL_GRACE
            + std::time::Duration::from_secs(2)
    }

    /// Builds a plain `ustar` archive at `archive` holding `files`.
    fn write_archive(archive: &Path, files: &[(&str, &str)]) {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let mut command = Command::new("tar");
        command
            .args(["--create", "--format=ustar", "--file"])
            .arg(archive)
            .arg("--directory")
            .arg(&src);
        for (name, content) in files {
            std::fs::write(src.join(name), content).unwrap();
            command.arg(name);
        }
        let status = command.status().unwrap();
        assert!(status.success(), "tar failed building the fixture archive");
    }

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

    /// A store entry sizing its download at 0 is capped as one with no size at
    /// all: the 2 MiB body unpacks under the no-size cap and the warning names
    /// that cap, where a twice-zero cap would have refused it at 1 MiB.
    #[test]
    fn an_unverified_download_sized_at_zero_is_capped_at_the_no_size_limit() {
        use crate::testutil::logs::{capture_logs, captured_logs};

        let dir = tempfile::tempdir().unwrap();
        let payload = "c".repeat(2 * 1024 * 1024);
        let archive = dir.path().join("FakeScheme.colors.tar");
        write_archive(&archive, &[("FakeScheme.colors", payload.as_str())]);
        let body = std::fs::read(&archive).unwrap();
        assert!(
            body.len() > 1024 * 1024,
            "the fixture is {} bytes, not past the 1 MiB cap a 0 would give it",
            body.len()
        );
        let url = serve_once(body);

        capture_logs();
        let client = reqwest::blocking::Client::new();
        let downloaded = download_package(
            &update_fixture(&url, None, Some(0)),
            &client,
            dir.path(),
            &(),
        )
        .expect("an unverified download whose entry sizes it at 0 was refused");

        let dest = dir.path().join("out");
        extract_archive(&downloaded, &dest).expect("the downloaded archive did not extract");
        assert_eq!(
            std::fs::read_to_string(dest.join("FakeScheme.colors")).unwrap(),
            payload,
            "the downloaded package did not unpack as the scheme"
        );

        let lines = captured_logs();
        let warning = format!(
            "the store advertises no checksum, so the download is unverified and capped at \
             {MAX_DOWNLOAD_BYTES} bytes"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("download: ") && line.ends_with(&warning)),
            "the unverified warning did not name the no-size cap ({MAX_DOWNLOAD_BYTES} bytes): \
             {lines:?}"
        );
    }

    /// A store entry's name holding a newline prints in its debug form, so the
    /// warning stays one log line instead of forging a second.
    #[test]
    fn a_component_name_holding_a_newline_prints_in_its_debug_form() {
        use crate::testutil::logs::captured_logs_while;

        let dir = tempfile::tempdir().unwrap();
        let url = serve_once(b"not read".to_vec());
        let mut update = update_fixture(&url, None, None);
        update.installed.name = "Fake\nScheme".to_string();
        let client = reqwest::blocking::Client::new();
        let lines = captured_logs_while(|| {
            let _downloaded = download_package(&update, &client, dir.path(), &());
        });

        let named: Vec<&String> = lines
            .iter()
            .filter(|line| {
                line.contains("advertises no checksum")
                    && (line.contains("Fake\nScheme") || line.contains("Fake\\nScheme"))
            })
            .collect();
        assert_eq!(
            named.len(),
            1,
            "the warning did not print exactly once: {lines:?}"
        );
        let expected = format!(
            "download: {:?}: the store advertises no checksum, so the download is \
             unverified and capped at {MAX_DOWNLOAD_BYTES} bytes",
            "Fake\nScheme"
        );
        assert_eq!(named[0], &expected, "the warning's line changed");
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
        assert_eq!(download_limit(None, Some(1000)), 2 * 1000 + 1024 * 1024);
        assert_eq!(download_limit(None, None), MAX_DOWNLOAD_BYTES);
        assert_eq!(download_limit(None, Some(0)), MAX_DOWNLOAD_BYTES);
    }

    #[test]
    fn an_unverified_download_advertising_one_and_a_half_gib_is_capped_at_the_extraction_cap() {
        assert_eq!(
            download_limit(None, Some(1536 * 1024 * 1024)),
            MAX_EXTRACTED_BYTES
        );
        // Control: just under the ceiling, twice the advertised size plus the slack still rules.
        assert_eq!(
            download_limit(None, Some(1_073_217_535)),
            2 * 1_073_217_535 + 1024 * 1024
        );
    }

    #[test]
    fn an_unverified_download_advertising_the_largest_size_is_capped_at_the_extraction_cap() {
        assert_eq!(download_limit(None, Some(u64::MAX)), MAX_EXTRACTED_BYTES);
        // Twice this size wraps to 0: a doubling that wraps would cap it at the 1 MiB slack.
        assert_eq!(download_limit(None, Some(1 << 63)), MAX_EXTRACTED_BYTES);
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

    const BSDTAR_EXIT_STATUS_CHILD: &str =
        "installer::download::tests::a_bsdtar_failure_with_empty_stderr_names_the_exit_status";

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

        let output = run_child(
            BSDTAR_EXIT_STATUS_CHILD,
            &bin,
            &[
                ("PU_TEST_BSDTAR_CHILD", "1"),
                ("PU_TEST_BSDTAR_ROOT", root.to_str().unwrap()),
            ],
        );
        assert!(
            output.status.success(),
            "child failed: {}\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    const HUNG_LISTING_CHILD: &str =
        "installer::download::tests::a_listing_that_prints_nothing_is_killed_at_the_bound";

    /// A fake `bsdtar` that prints no listing line, keeps its stdout open
    /// through a helper of its own and sleeps, so only a kill of the whole
    /// process group ends the caller's read.
    const HUNG_LISTING_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-t" ]; then
  sh -c 'echo $$ > "$PU_TEST_BSDTAR_HELPER_PID"; exec sleep 10' &
  echo $$ > "$PU_TEST_BSDTAR_PID"
  exec sleep 10
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    #[test]
    fn a_listing_that_prints_nothing_is_killed_at_the_bound() {
        if std::env::var_os("PU_TEST_HUNG_LISTING").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let archive = root.join("package.tar");
            let dest = root.join("out");
            let started = std::time::Instant::now();

            let refusal = extract_archive(&archive, &dest).unwrap_err();
            let elapsed = started.elapsed();

            assert_eq!(refusal.kind, FailureKind::Extract);
            assert_eq!(refusal.message, "bsdtar timed out after 1 s");
            assert!(
                elapsed < within_bound(1),
                "the listing read was not ended at its bound: {elapsed:?}"
            );
            assert!(!dest.exists(), "a hung listing still extracted the archive");
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), HUNG_LISTING_BSDTAR);
        write_archive(
            &root.join("package.tar"),
            &[("Fake.colors", "[General]\nName=Fake\n")],
        );
        let tool_pid = root.join("tool-pid");
        let helper_pid = root.join("helper-pid");
        let done = root.join("child-done");

        let output = run_child(
            HUNG_LISTING_CHILD,
            &bin,
            &[
                ("PU_TEST_HUNG_LISTING", "1"),
                ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000"),
                ("PU_TEST_REAL_BSDTAR", real_bsdtar().to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_BSDTAR_PID", tool_pid.to_str().unwrap()),
                ("PU_TEST_BSDTAR_HELPER_PID", helper_pid.to_str().unwrap()),
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
        for pid in [&tool_pid, &helper_pid] {
            assert!(
                !recorded_still_running(pid),
                "a fake bsdtar outlived the bound and is still running"
            );
        }
    }

    const ESCAPED_HOLDER_CHILD: &str = "installer::download::tests::a_listing_whose_stdout_an_outside_process_holds_is_refused_at_the_bound";

    /// A fake `bsdtar` whose listing starts a helper in a session of its own, so
    /// the group kill cannot reach it, and that helper keeps the listing's
    /// stdout open after the real tool has exited.
    const ESCAPED_HOLDER_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-t" ]; then
  setsid sh -c 'echo $$ > "$PU_TEST_BSDTAR_HELPER_PID"; exec sleep 12' 2>/dev/null &
  exec "$PU_TEST_REAL_BSDTAR" "$@"
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    #[test]
    fn a_listing_whose_stdout_an_outside_process_holds_is_refused_at_the_bound() {
        if std::env::var_os("PU_TEST_ESCAPED_HOLDER").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let archive = root.join("package.tar");
            let dest = root.join("out");
            let started = std::time::Instant::now();

            let refusal = extract_archive(&archive, &dest).unwrap_err();
            let elapsed = started.elapsed();

            assert_eq!(refusal.kind, FailureKind::Extract, "{}", refusal.message);
            assert_eq!(refusal.message, "bsdtar timed out after 1 s");
            assert!(
                elapsed < within_bound(1),
                "the listing was held by a process outside its group: {elapsed:?}"
            );
            assert!(!dest.exists(), "a hung listing still extracted the archive");
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), ESCAPED_HOLDER_BSDTAR);
        write_archive(
            &root.join("package.tar"),
            &[("Fake.colors", "[General]\nName=Fake\n")],
        );
        let helper_pid = root.join("helper-pid");
        let done = root.join("child-done");

        let output = run_child(
            ESCAPED_HOLDER_CHILD,
            &bin,
            &[
                ("PU_TEST_ESCAPED_HOLDER", "1"),
                ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000"),
                ("PU_TEST_REAL_BSDTAR", real_bsdtar().to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_BSDTAR_HELPER_PID", helper_pid.to_str().unwrap()),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
            ],
        );

        let survived = stop_recorded(&helper_pid);
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            survived,
            "the stdout holder did not outlive the call, so the fixture tests nothing"
        );
    }

    const STDIN_CHILD: &str =
        "installer::download::tests::both_bsdtar_calls_get_stdin_from_dev_null";

    /// A fake `bsdtar` that records what each call's fd 0 is before running the
    /// real tool.
    const STDIN_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-t" ]; then
  readlink "/proc/$$/fd/0" > "$PU_TEST_LISTING_STDIN_SEEN"
fi
if [ "$1" = "-xf" ]; then
  readlink "/proc/$$/fd/0" > "$PU_TEST_EXTRACTION_STDIN_SEEN"
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    /// Both `bsdtar` calls get their stdin from `/dev/null` rather than the
    /// caller's own, so no archive can prompt on or read it.
    #[test]
    fn both_bsdtar_calls_get_stdin_from_dev_null() {
        if std::env::var_os("PU_TEST_STDIN").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let archive = root.join("package.tar");
            let dest = root.join("out");

            extract_archive(&archive, &dest).expect("a plain archive was refused");

            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), STDIN_BSDTAR);
        write_archive(
            &root.join("package.tar"),
            &[("Fake.colors", "[General]\nName=Fake\n")],
        );
        let stdin_file = root.join("stdin.txt");
        std::fs::write(&stdin_file, "caller input\n").unwrap();
        let listing_seen = root.join("listing-stdin");
        let extraction_seen = root.join("extraction-stdin");
        let done = root.join("child-done");
        let output = child_command(
            STDIN_CHILD,
            &bin,
            &[
                ("PU_TEST_STDIN", "1"),
                ("PU_TEST_REAL_BSDTAR", real_bsdtar().to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_LISTING_STDIN_SEEN", listing_seen.to_str().unwrap()),
                (
                    "PU_TEST_EXTRACTION_STDIN_SEEN",
                    extraction_seen.to_str().unwrap(),
                ),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
            ],
        )
        .stdin(std::fs::File::open(&stdin_file).unwrap())
        .output()
        .unwrap();
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
        for (call, path) in [("extraction", &extraction_seen), ("listing", &listing_seen)] {
            let seen = std::fs::read_to_string(path).unwrap_or_default();
            assert_eq!(
                seen.trim(),
                "/dev/null",
                "the {call} call read its stdin from {:?}",
                seen.trim()
            );
        }
    }

    const HUNG_EXTRACTION_CHILD: &str =
        "installer::download::tests::an_extraction_that_hangs_is_refused_at_the_bound";

    /// A fake `bsdtar` that lists through the real tool and sleeps in place of
    /// the extraction, recording its pid.
    const HUNG_EXTRACTION_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-xf" ]; then
  echo $$ > "$PU_TEST_BSDTAR_PID"
  exec sleep 10
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    #[test]
    fn an_extraction_that_hangs_is_refused_at_the_bound() {
        if std::env::var_os("PU_TEST_HUNG_EXTRACTION").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let archive = root.join("package.tar");
            let dest = root.join("out");
            let started = std::time::Instant::now();

            let refusal = extract_archive(&archive, &dest).unwrap_err();
            let elapsed = started.elapsed();

            assert_eq!(refusal.kind, FailureKind::Extract);
            assert_eq!(refusal.message, "bsdtar timed out after 1 s");
            assert!(
                !refusal.process_may_still_run,
                "a killed extraction's process was read as possibly running"
            );
            assert!(
                elapsed < within_bound(1),
                "the extraction outlived its bound: {elapsed:?}"
            );
            assert!(
                !dest.join("Fake.colors").exists(),
                "a timed-out extraction wrote the archive out"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), HUNG_EXTRACTION_BSDTAR);
        write_archive(
            &root.join("package.tar"),
            &[("Fake.colors", "[General]\nName=Fake\n")],
        );
        let tool_pid = root.join("tool-pid");
        let done = root.join("child-done");

        let output = run_child(
            HUNG_EXTRACTION_CHILD,
            &bin,
            &[
                ("PU_TEST_HUNG_EXTRACTION", "1"),
                ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000"),
                ("PU_TEST_REAL_BSDTAR", real_bsdtar().to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_BSDTAR_PID", tool_pid.to_str().unwrap()),
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
        assert!(
            !recorded_still_running(&tool_pid),
            "a fake bsdtar outlived the bound and is still running"
        );
    }

    const UNBOUNDED_EXTRACTION_CHILD: &str =
        "installer::download::tests::a_hung_extraction_fails_its_component_and_the_run_goes_on";

    /// A fake `bsdtar` that hangs only on the extraction of the archive named
    /// `Slow` and runs the real tool for every other call.
    const SLOW_EXTRACTION_BSDTAR: &str = r#"#!/bin/sh
hang=0
for arg in "$@"; do
  case "$arg" in
    *Slow.colors.tar*) hang=1 ;;
  esac
done
if [ "$hang" = 1 ] && [ "$1" = "-xf" ]; then
  echo $$ > "$PU_TEST_BSDTAR_PID"
  exec sleep 10
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    /// A color scheme installed in the sandbox, named `name`.
    fn scheme_at(root: &Path, name: &str) -> InstalledComponent {
        let path = root
            .join("data")
            .join("color-schemes")
            .join(format!("{name}.colors"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old").unwrap();
        InstalledComponent {
            name: name.to_string(),
            directory_name: name.to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::ColorScheme,
            path,
            is_system: false,
            release_date: String::new(),
        }
    }

    /// A component whose extraction passes the bound fails as `Extract`,
    /// naming the bound, and the run installs the next component anyway.
    #[test]
    fn a_hung_extraction_fails_its_component_and_the_run_goes_on() {
        if std::env::var_os("PU_TEST_UNBOUNDED_EXTRACTION").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let base = std::env::var("PU_TEST_EXTRACT_STORE_URL").unwrap();
            let slow = scheme_at(&root, "Slow");
            let fast = scheme_at(&root, "Fast");
            let updates = [
                crate::types::AvailableUpdate::builder(slow.clone(), 1, "2.0")
                    .download_url(format!("{base}/dl/Slow.colors.tar"))
                    .build(),
                crate::types::AvailableUpdate::builder(fast.clone(), 2, "2.0")
                    .download_url(format!("{base}/dl/Fast.colors.tar"))
                    .build(),
            ];
            let config = crate::Config::new()
                .with_api_url(&base)
                .with_skip_plasma_detection(true)
                .with_inhibit_idle(false)
                .with_threads(1);
            let started = std::time::Instant::now();

            let result = crate::install(&updates, &config, &()).unwrap();
            let elapsed = started.elapsed();

            assert_eq!(
                result.failed.len(),
                1,
                "the hung extraction did not fail exactly its own component: {result:?}"
            );
            let failure = &result.failed[0];
            assert_eq!(failure.component.name, "Slow", "{}", failure.message);
            assert_eq!(failure.kind, FailureKind::Extract, "{}", failure.message);
            assert_eq!(failure.message, "bsdtar timed out after 1 s");
            assert_eq!(
                result.succeeded.len(),
                1,
                "the run did not go on to the next component: {result:?}"
            );
            assert_eq!(result.succeeded[0].name, "Fast");
            assert_eq!(
                std::fs::read_to_string(&slow.path).unwrap(),
                "old",
                "the failed component's file changed"
            );
            assert_eq!(
                std::fs::read_to_string(&fast.path).unwrap(),
                "new",
                "the next component was not installed"
            );
            assert!(
                elapsed < within_bound(1),
                "the run did not go on past the hung extraction: {elapsed:?}"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), SLOW_EXTRACTION_BSDTAR);
        for sub in ["home", "data", "config", "cache", "runtime", "tmp"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        write_archive(&root.join("Slow.colors.tar"), &[("Slow.colors", "new")]);
        write_archive(&root.join("Fast.colors.tar"), &[("Fast.colors", "new")]);
        let slow_body =
            String::from_utf8(std::fs::read(root.join("Slow.colors.tar")).unwrap()).unwrap();
        let fast_body =
            String::from_utf8(std::fs::read(root.join("Fast.colors.tar")).unwrap()).unwrap();
        let (base, _) = crate::testutil::serve(move |target| {
            if target.ends_with("/dl/Slow.colors.tar") {
                crate::testutil::ok(slow_body.clone())
            } else {
                crate::testutil::ok(fast_body.clone())
            }
        });
        let tool_pid = root.join("tool-pid");
        let done = root.join("child-done");
        let path = |sub: &str| root.join(sub).to_str().unwrap().to_string();
        let envs = [
            ("PU_TEST_UNBOUNDED_EXTRACTION", "1".to_string()),
            ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000".to_string()),
            (
                "PU_TEST_REAL_BSDTAR",
                real_bsdtar().to_str().unwrap().to_string(),
            ),
            ("PU_TEST_ROOT", root.to_str().unwrap().to_string()),
            ("PU_TEST_EXTRACT_STORE_URL", base),
            ("PU_TEST_BSDTAR_PID", tool_pid.to_str().unwrap().to_string()),
            ("PU_TEST_CHILD_DONE", done.to_str().unwrap().to_string()),
            ("HOME", path("home")),
            ("XDG_DATA_HOME", path("data")),
            ("XDG_CONFIG_HOME", path("config")),
            ("XDG_CACHE_HOME", path("cache")),
            ("XDG_RUNTIME_DIR", path("runtime")),
            ("XDG_DATA_DIRS", path("data")),
            ("TMPDIR", path("tmp")),
        ];
        let refs: Vec<(&str, &str)> = envs
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();

        let output = run_child(UNBOUNDED_EXTRACTION_CHILD, &bin, &refs);

        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            !recorded_still_running(&tool_pid),
            "a fake bsdtar outlived the bound and is still running"
        );
    }

    const CLOSED_STDOUT_CHILD: &str = "installer::download::tests::a_listing_that_closes_its_stdout_and_hangs_is_refused_at_the_bound";

    /// A fake `bsdtar` that prints one listing line, closes its stdout and
    /// sleeps: the read ends at once while the child holds on past the bound.
    const CLOSED_STDOUT_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-t" ]; then
  echo "-rw-r--r--  0 0 0 4 Nov  2 10:00 f"
  exec 1>&-
  echo $$ > "$PU_TEST_BSDTAR_PID"
  exec sleep 10
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    #[test]
    fn a_listing_that_closes_its_stdout_and_hangs_is_refused_at_the_bound() {
        if std::env::var_os("PU_TEST_CLOSED_STDOUT").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let archive = root.join("package.tar");
            let dest = root.join("out");
            let started = std::time::Instant::now();

            let refusal = extract_archive(&archive, &dest).unwrap_err();
            let elapsed = started.elapsed();

            assert_eq!(refusal.kind, FailureKind::Extract, "{}", refusal.message);
            assert_eq!(refusal.message, "bsdtar timed out after 1 s");
            assert!(
                elapsed < within_bound(1),
                "a listing that outlived its stdout held the call: {elapsed:?}"
            );
            assert!(!dest.exists(), "a hung listing still extracted the archive");
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), CLOSED_STDOUT_BSDTAR);
        write_archive(
            &root.join("package.tar"),
            &[("Fake.colors", "[General]\nName=Fake\n")],
        );
        let tool_pid = root.join("tool-pid");
        let done = root.join("child-done");

        let output = run_child(
            CLOSED_STDOUT_CHILD,
            &bin,
            &[
                ("PU_TEST_CLOSED_STDOUT", "1"),
                ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000"),
                ("PU_TEST_REAL_BSDTAR", real_bsdtar().to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_BSDTAR_PID", tool_pid.to_str().unwrap()),
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
        assert!(
            !recorded_still_running(&tool_pid),
            "a fake bsdtar outlived the bound and is still running"
        );
    }

    const STALE_STDERR_CHILD: &str = "installer::download::tests::a_stderr_reader_that_outlives_the_listing_does_not_hold_the_call";

    /// A fake `bsdtar` whose listing forks a helper holding stderr open for
    /// longer than the bound, then lists through the real tool: the listing
    /// succeeds, but its stderr pipe outlives the tool that opened it.
    const STALE_STDERR_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-t" ]; then
  sh -c 'echo $$ > "$PU_TEST_BSDTAR_HELPER_PID"; exec sleep 12 >/dev/null' &
  exec "$PU_TEST_REAL_BSDTAR" "$@"
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    #[test]
    fn a_stderr_reader_that_outlives_the_listing_does_not_hold_the_call() {
        if std::env::var_os("PU_TEST_STALE_STDERR").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let archive = root.join("package.tar");
            let dest = root.join("out");
            let started = std::time::Instant::now();

            extract_archive(&archive, &dest).expect("a listed archive was refused");
            let elapsed = started.elapsed();

            assert!(
                elapsed < within_bound(1),
                "a stderr reader that outlived the listing held the call: {elapsed:?}"
            );
            assert!(
                dest.join("Fake.colors").exists(),
                "the listed archive was not extracted"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), STALE_STDERR_BSDTAR);
        write_archive(
            &root.join("package.tar"),
            &[("Fake.colors", "[General]\nName=Fake\n")],
        );
        let helper_pid = root.join("helper-pid");
        let done = root.join("child-done");

        let output = run_child(
            STALE_STDERR_CHILD,
            &bin,
            &[
                ("PU_TEST_STALE_STDERR", "1"),
                ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000"),
                ("PU_TEST_REAL_BSDTAR", real_bsdtar().to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_BSDTAR_HELPER_PID", helper_pid.to_str().unwrap()),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
            ],
        );

        let survived = stop_recorded(&helper_pid);
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            survived,
            "the stderr holder did not outlive the call, so the fixture tests nothing"
        );
    }

    const GROUP_LEAVING_LISTING_CHILD: &str =
        "installer::download::tests::a_listing_child_the_group_kill_misses_does_not_hold_the_call";

    /// A fake `bsdtar` that moves itself into its parent's process group, out
    /// of the group kill's reach, and keeps its stdout and stderr open, so only
    /// the wait for it could hold the call.
    const GROUP_LEAVING_LISTING_BSDTAR: &str = r#"#!/bin/sh
if [ "$1" = "-t" ]; then
  exec perl -e 'setpgrp(0, getpgrp(getppid())) or die; open(my $f, ">", $ENV{PU_TEST_BSDTAR_PID}) or die; print $f "$$\n"; close($f); sleep 30'
fi
exec "$PU_TEST_REAL_BSDTAR" "$@"
"#;

    #[test]
    fn a_listing_child_the_group_kill_misses_does_not_hold_the_call() {
        if std::env::var_os("PU_TEST_GROUP_LEAVING_LISTING").is_some() {
            let root = PathBuf::from(std::env::var("PU_TEST_ROOT").unwrap());
            let archive = root.join("package.tar");
            let dest = root.join("out");
            let started = std::time::Instant::now();

            let refusal = extract_archive(&archive, &dest).unwrap_err();
            let elapsed = started.elapsed();

            // The 1 s timeout, the reap bound and the grace, plus slack for a
            // loaded machine; the stub sleeps 30 s.
            let bound = within_bound(1) + super::super::install::REAP_BOUND;
            assert_eq!(refusal.kind, FailureKind::Extract, "{}", refusal.message);
            assert_eq!(refusal.message, "bsdtar timed out after 1 s");
            assert!(
                elapsed < bound,
                "the wait after the SIGKILL held the call: {elapsed:?}"
            );
            std::fs::write(std::env::var_os("PU_TEST_CHILD_DONE").unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("bsdtar"), GROUP_LEAVING_LISTING_BSDTAR);
        write_archive(&root.join("package.tar"), &[("Fake.colors", "x")]);
        let tool_pid = root.join("tool-pid");
        let done = root.join("child-done");

        let output = run_child(
            GROUP_LEAVING_LISTING_CHILD,
            &bin,
            &[
                ("PU_TEST_GROUP_LEAVING_LISTING", "1"),
                ("PU_TEST_BSDTAR_TIMEOUT_MS", "1000"),
                ("PU_TEST_REAL_BSDTAR", real_bsdtar().to_str().unwrap()),
                ("PU_TEST_ROOT", root.to_str().unwrap()),
                ("PU_TEST_BSDTAR_PID", tool_pid.to_str().unwrap()),
                ("PU_TEST_CHILD_DONE", done.to_str().unwrap()),
            ],
        );

        let survived = stop_recorded(&tool_pid);
        assert!(
            output.status.success() && done.exists(),
            "child run failed ({}; body completed: {})\n--- stderr\n{}",
            output.status,
            done.exists(),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            survived,
            "the group-leaving child did not outlive the call, so the fixture tests nothing"
        );
    }

    #[test]
    fn served_file_name_drops_the_query_and_decodes_escapes() {
        let cases = [
            ("http://host/dl/forest.png", "forest.png"),
            ("http://host/dl/forest.png?dl=1", "forest.png"),
            ("http://host/dl/forest.png#frag", "forest.png"),
            ("http://host/dl/forest.png?dl=1#frag", "forest.png"),
            ("http://host/dl/Forest%20Night.png", "Forest Night.png"),
            ("http://host/dl/caf%C3%A9.png", "caf\u{e9}.png"),
            ("http://host/dl/a%2Fb.png", "a/b.png"),
            ("http://host/dl/a%2fb.png", "a/b.png"),
            ("http://host/dl/a%00b.png", "a\0b.png"),
            ("http://host/dl/a%zz.png", "a%zz.png"),
            ("http://host/dl/a%2.png", "a%2.png"),
            ("http://host/dl/forest%2", "forest%2"),
            ("http://host/dl/forest%", "forest%"),
            ("http://host/dl/", ""),
            ("http://host", ""),
            ("http://host?dl=1", ""),
        ];
        for (url, expected) in cases {
            assert_eq!(
                super::served_file_name(url),
                Ok(expected.to_string()),
                "{url}"
            );
        }
        assert_eq!(
            super::served_file_name("http://host/dl/%FF.png"),
            Err("%FF.png".to_string())
        );
        assert_eq!(
            super::served_file_name("http://host/dl/%C3.png"),
            Err("%C3.png".to_string())
        );
    }
}
