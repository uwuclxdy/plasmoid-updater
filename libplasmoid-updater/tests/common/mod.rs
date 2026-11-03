//! Harness for the public-API tests: a sandboxed child process per test and a
//! local fake KDE Store.
//!
//! The library resolves every path through `HOME` and `XDG_*`, and setting
//! environment variables in-process needs `unsafe`, which the workspace forbids.
//! So each test runs twice: the parent builds the fixture and the fake store,
//! then re-runs its own test binary filtered to that one test with the sandbox
//! environment set; the child calls the library and asserts on its results.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// Every test crate compiles this module on its own and uses a different part.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const SCHEME_ID: u64 = 999_001;
pub const SCHEME_NAME: &str = "Fake Scheme";
pub const SCHEME_FILE: &str = "FakeScheme.colors";
pub const OLD_VERSION: &str = "1.0";
pub const NEW_VERSION: &str = "2.0";
pub const OLD_CONTENT: &str = "[General]\nName=Fake Scheme\n# old\n";
pub const NEW_CONTENT: &str = "[General]\nName=Fake Scheme\n# new\n";
/// KDE Store category id for color schemes.
const COLOR_SCHEME_CATEGORY: u16 = 112;

const CHILD_FLAG: &str = "PLASMOID_UPDATER_TEST_CHILD";
const STORE_URL_VAR: &str = "PLASMOID_UPDATER_TEST_STORE_URL";
const MARKER_VAR: &str = "PLASMOID_UPDATER_TEST_MARKER";

/// What a child process sees: the fake store's base URL.
pub struct Child {
    pub store_url: String,
    marker: PathBuf,
}

impl Child {
    /// Records that the child body ran to completion. Without it the parent
    /// cannot tell a passing child from a filter that matched no test.
    pub fn done(self) {
        std::fs::write(&self.marker, b"ok").unwrap();
    }
}

/// Returns the child context when this process is a sandboxed child run.
pub fn child() -> Option<Child> {
    std::env::var_os(CHILD_FLAG)?;
    Some(Child {
        store_url: std::env::var(STORE_URL_VAR).unwrap(),
        marker: PathBuf::from(std::env::var_os(MARKER_VAR).unwrap()),
    })
}

/// A fresh `HOME` with every XDG dir the library reads pointed inside it.
pub struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "home",
            "data",
            "config",
            "cache",
            "runtime",
            "tmp",
            "pu-config",
        ] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        Self { root }
    }

    pub fn data_home(&self) -> PathBuf {
        self.root.path().join("data")
    }

    /// The sandbox's `HOME`, the parent of every `XDG_*` directory.
    pub fn home_dir(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// The sandbox's `XDG_CACHE_HOME`, where backups land.
    pub fn cache_dir(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    /// The sandbox's `TMPDIR`, where downloads are unpacked.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.path().join("tmp")
    }

    pub fn scheme_path(&self) -> PathBuf {
        self.data_home().join("color-schemes").join(SCHEME_FILE)
    }

    pub fn registry_path(&self) -> PathBuf {
        self.data_home()
            .join("knewstuff3")
            .join("colorschemes.knsregistry")
    }

    /// Installs the fake color scheme at `OLD_VERSION`, registered in the
    /// KNewStuff registry the way Discover records a store install.
    pub fn install_old_scheme(&self) {
        let scheme = self.scheme_path();
        std::fs::create_dir_all(scheme.parent().unwrap()).unwrap();
        std::fs::write(&scheme, OLD_CONTENT).unwrap();

        let registry = self.registry_path();
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        std::fs::write(
            &registry,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
  <stuff category="{COLOR_SCHEME_CATEGORY}">
    <name>{SCHEME_NAME}</name>
    <providerid>api.kde-look.org</providerid>
    <version>{OLD_VERSION}</version>
    <installedfile>{installed}</installedfile>
    <id>{SCHEME_ID}</id>
    <status>installed</status>
  </stuff>
</hotnewstuffregistry>
"#,
                installed = scheme.display()
            ),
        )
        .unwrap();
    }

    pub fn registry_content(&self) -> String {
        std::fs::read_to_string(self.registry_path()).unwrap()
    }

    /// The plasmoid-updater config dir the sandbox points
    /// `PLASMOID_UPDATER_CONFIG_DIR` at.
    pub fn config_dir(&self) -> PathBuf {
        self.root.path().join("pu-config")
    }

    /// A directory searched before the inherited `PATH`, where a test puts
    /// fake external tools.
    pub fn bin_dir(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    /// Writes an executable `sh` script named `name` into [`Self::bin_dir`].
    pub fn fake_tool(&self, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.bin_dir();
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A command for `program` with a cleared environment holding only `PATH`
    /// (with [`Self::bin_dir`] first) and the sandbox's `HOME`, `XDG_*`,
    /// `TMPDIR` and config dir.
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let root = self.root.path();
        let mut path = std::ffi::OsString::from(self.bin_dir());
        if let Some(inherited) = std::env::var_os("PATH") {
            path.push(":");
            path.push(inherited);
        }
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", path)
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root.join("tmp"))
            .env("PLASMOID_UPDATER_CONFIG_DIR", self.config_dir());
        // An instrumented coverage run names its profile output here; without
        // it the child's coverage is lost.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command
    }

    /// Re-runs this test binary filtered to `test` inside the sandbox, and
    /// panics with the child's output unless it ran the test body to the end.
    pub fn run_child(&self, test: &str, store_url: &str) {
        self.run_child_with_data_home(test, store_url, &self.root.path().join("data"));
    }

    /// Like [`run_child`](Self::run_child), with `XDG_DATA_HOME` overridden to
    /// `data_home` (the later `env` wins over [`command`](Self::command)'s
    /// default), for a test that needs a differently-spelled data directory.
    pub fn run_child_with_data_home(&self, test: &str, store_url: &str, data_home: &Path) {
        let marker = self.root.path().join("child-done");
        let output = self
            .command(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_FLAG, "1")
            .env(STORE_URL_VAR, store_url)
            .env(MARKER_VAR, &marker)
            .env("XDG_DATA_HOME", data_home)
            .output()
            .unwrap();
        assert!(
            output.status.success() && marker.exists(),
            "child run of {test} failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            marker.exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// A widget entry the fake category listing serves when the query names its
/// subcategory id.
#[derive(Clone, Copy)]
pub struct CatalogWidget {
    pub id: u64,
    pub name: &'static str,
    pub type_id: u16,
    pub version: &'static str,
}

/// What `<meta><totalitems>` reports on a catalog page.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TotalItems {
    /// The page's own item count: one entry on a listing page, zero otherwise.
    Default,
    /// A fixed count on every page.
    Items(u32),
}

/// One store entry the fake store serves beyond the fake color scheme: listed
/// in the catalog under `type_id`, answered by its per-id lookup, and
/// downloadable as a `.tar.gz` of `files`.
pub struct ExtraEntry {
    pub id: u64,
    pub name: &'static str,
    pub type_id: u16,
    pub version: &'static str,
    /// Archive members, as `(relative path, contents)`.
    pub files: &'static [(&'static str, &'static str)],
}

/// What the fake store advertises for the fake color scheme.
pub struct Listing {
    pub version: &'static str,
    /// MD5 the store advertises; `None` serves the real archive's MD5.
    pub md5_override: Option<&'static str>,
    /// Whether the category listing includes the entry; `false` leaves the
    /// per-id lookup as the only way to reach it.
    pub in_catalog: bool,
    /// Whether the per-id lookup answers `503 Service Unavailable`.
    pub per_id_fails: bool,
    /// How many per-id lookups answer `503` before the lookup succeeds.
    pub per_id_fails_first: usize,
    /// Whether the per-id lookup closes the connection without answering.
    pub per_id_drops: bool,
    /// Whether the per-id lookup redirects to a port nothing listens on, so
    /// following the redirect fails to connect while the catalog answered.
    pub per_id_connects_nowhere: bool,
    /// Whether the per-id lookup answers with a `200` status line and then
    /// drops the connection mid-body.
    pub per_id_drops_mid_body: bool,
    /// Whether the catalog listing redirects to a port nothing listens on, so
    /// following the redirect fails to connect while the store is up.
    pub catalog_connects_nowhere: bool,
    /// Whether the first catalog page answers `200 OK` with a body that is
    /// not OCS XML.
    pub catalog_garbage: bool,
    /// Whether a download request is held open for a minute before any byte
    /// of the answer is sent.
    pub download_stalls: bool,
    /// A widget entry the category listing serves when the query names its
    /// subcategory id.
    pub widget: Option<CatalogWidget>,
    /// The `<totalitems>` every catalog page reports.
    pub total_items: TotalItems,
    /// The color scheme's `<download_version1>`; `None` serves `version`.
    pub link_version: Option<&'static str>,
    /// Whether the color scheme's entry omits `<downloadmd5sum1>`.
    pub no_md5: bool,
    /// The color scheme's `<downloadsize1>` in KiB; `None` serves the
    /// archive's real size.
    pub size_kb_override: Option<u64>,
    /// Zero bytes appended to the served color scheme archive.
    pub download_padding: usize,
    /// When set, the widget download is a Plasma widget package whose
    /// `KPlugin.Id` is this value, named and versioned as `widget` says.
    pub widget_package_id: Option<&'static str>,
    /// When set, the widget package's `metadata.json` is a symlink to a file
    /// outside the package, beside a regular `metadata.desktop`.
    pub widget_metadata_symlink: bool,
    /// When set, the widget package carries a chain of links that each stay
    /// inside the package root on paper but resolve outside it.
    pub widget_package_link: bool,
    /// Store entries served beside the color scheme, each with its own archive.
    pub extras: Vec<ExtraEntry>,
}

impl Default for Listing {
    fn default() -> Self {
        Self {
            version: NEW_VERSION,
            md5_override: None,
            in_catalog: true,
            per_id_fails: false,
            per_id_fails_first: 0,
            per_id_drops: false,
            per_id_connects_nowhere: false,
            per_id_drops_mid_body: false,
            catalog_connects_nowhere: false,
            catalog_garbage: false,
            download_stalls: false,
            widget: None,
            total_items: TotalItems::Default,
            link_version: None,
            no_md5: false,
            size_kb_override: None,
            download_padding: 0,
            widget_package_id: None,
            widget_metadata_symlink: false,
            widget_package_link: false,
            extras: Vec::new(),
        }
    }
}

/// A local HTTP server speaking enough of the KDE Store's OCS v1 API for the
/// library: the category listing, the per-id content lookup, and the download.
pub struct FakeStore {
    base: String,
    served: Arc<Served>,
}

impl FakeStore {
    pub fn start(listing: Listing) -> Self {
        let mut archive = scheme_archive();
        let size_kb = listing
            .size_kb_override
            .unwrap_or_else(|| archive.len().div_ceil(1024) as u64);
        archive.resize(archive.len() + listing.download_padding, 0);
        let md5 = listing
            .md5_override
            .map_or_else(|| format!("{:x}", md5::compute(&archive)), str::to_string);
        let md5_field = if listing.no_md5 {
            String::new()
        } else {
            format!("<downloadmd5sum1>{md5}</downloadmd5sum1>")
        };
        let widget_archive = listing
            .widget
            .zip(listing.widget_package_id)
            .map(|(widget, id)| {
                widget_archive(
                    id,
                    widget.name,
                    widget.version,
                    listing.widget_metadata_symlink,
                    listing.widget_package_link,
                )
            });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = format!("http://127.0.0.1:{port}");
        let dead_port = {
            let probe = TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let entry = format!(
            "<content details=\"full\"><id>{SCHEME_ID}</id><name>{SCHEME_NAME}</name>\
             <version>{version}</version><typeid>{COLOR_SCHEME_CATEGORY}</typeid>\
             <changed>2026-10-01T00:00:00+00:00</changed>\
             <downloadlink1>{host}/dl/fake-scheme.tar.gz</downloadlink1>\
             <download_version1>{link_version}</download_version1>\
             {md5_field}\
             <downloadsize1>{size_kb}</downloadsize1></content>",
            version = listing.version,
            link_version = listing.link_version.unwrap_or(listing.version),
        );
        let widget = listing.widget.map(|widget| {
            let entry = format!(
                "<content details=\"full\"><id>{}</id><name>{}</name>\
                 <version>{}</version><typeid>{}</typeid>\
                 <changed>2026-10-01T00:00:00+00:00</changed>\
                 <downloadlink1>{host}/dl/fake-widget.tar.gz</downloadlink1>\
                 <download_version1>{}</download_version1></content>",
                widget.id, widget.name, widget.version, widget.type_id, widget.version,
            );
            (widget.type_id, entry)
        });
        let extras: Vec<ServedExtra> = listing
            .extras
            .iter()
            .map(|extra| ServedExtra {
                id: extra.id,
                type_id: extra.type_id,
                xml: extra_entry_xml(&host, extra),
                archive: files_archive(extra.files),
            })
            .collect();
        let state = Arc::new(Served {
            entry,
            widget,
            archive,
            widget_archive,
            extras,
            in_catalog: listing.in_catalog,
            per_id_fails: listing.per_id_fails,
            per_id_fails_left: AtomicUsize::new(listing.per_id_fails_first),
            per_id_drops: listing.per_id_drops,
            per_id_connects_nowhere: listing.per_id_connects_nowhere,
            per_id_drops_mid_body: listing.per_id_drops_mid_body,
            catalog_connects_nowhere: listing.catalog_connects_nowhere,
            catalog_garbage: listing.catalog_garbage,
            download_stalls: listing.download_stalls,
            total_items: listing.total_items,
            dead_port,
            catalog_requests: AtomicUsize::new(0),
        });
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self {
            base: format!("{host}/ocs/v1"),
            served: state,
        }
    }

    /// The OCS base URL to hand to `Config::with_api_url`.
    pub fn url(&self) -> &str {
        &self.base
    }

    /// How many catalog listing requests the store has answered.
    pub fn catalog_requests(&self) -> usize {
        self.served.catalog_requests.load(Ordering::SeqCst)
    }
}

/// A store URL nothing listens on.
pub fn dead_store_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/ocs/v1")
}

/// One extra entry the fake store answers with, its XML and archive prepared.
struct ServedExtra {
    id: u64,
    type_id: u16,
    xml: String,
    archive: Vec<u8>,
}

struct Served {
    entry: String,
    widget: Option<(u16, String)>,
    archive: Vec<u8>,
    widget_archive: Option<Vec<u8>>,
    extras: Vec<ServedExtra>,
    in_catalog: bool,
    per_id_fails: bool,
    per_id_fails_left: AtomicUsize,
    per_id_drops: bool,
    per_id_connects_nowhere: bool,
    per_id_drops_mid_body: bool,
    catalog_connects_nowhere: bool,
    catalog_garbage: bool,
    download_stalls: bool,
    total_items: TotalItems,
    dead_port: u16,
    catalog_requests: AtomicUsize,
}

fn serve(mut stream: TcpStream, served: &Served) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) | Err(_) => break,
            Ok(_) if header == "\r\n" || header == "\n" => break,
            Ok(_) => {}
        }
    }
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let entry = served.entry.as_str();
    let mut location: Option<String> = None;
    let (status, body): (&str, Vec<u8>) = if target == "/dl/fake-widget.tar.gz"
        && let Some(widget_archive) = &served.widget_archive
    {
        ("200 OK", widget_archive.clone())
    } else if let Some(extra) = served
        .extras
        .iter()
        .find(|extra| target == format!("/dl/extra-{}.tar.gz", extra.id))
    {
        ("200 OK", extra.archive.clone())
    } else if target.starts_with("/dl/") {
        if served.download_stalls {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        ("200 OK", served.archive.clone())
    } else if target == format!("/ocs/v1/content/data/{SCHEME_ID}") {
        let fail_now = served
            .per_id_fails_left
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if served.per_id_connects_nowhere {
            location = Some(format!("http://127.0.0.1:{}/nope", served.dead_port));
            ("302 Found", Vec::new())
        } else if served.per_id_drops_mid_body {
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(b"<ocs><meta>");
            return;
        } else if served.per_id_drops {
            return;
        } else if served.per_id_fails || fail_now {
            ("503 Service Unavailable", Vec::new())
        } else {
            ("200 OK", ocs(Some(1), entry).into_bytes())
        }
    } else if let Some(extra) = served
        .extras
        .iter()
        .find(|extra| target == format!("/ocs/v1/content/data/{}", extra.id))
    {
        ("200 OK", ocs(Some(1), &extra.xml).into_bytes())
    } else if let Some(query) = target.strip_prefix("/ocs/v1/content/data?") {
        served.catalog_requests.fetch_add(1, Ordering::SeqCst);
        if served.catalog_connects_nowhere {
            location = Some(format!("http://127.0.0.1:{}/nope", served.dead_port));
            ("302 Found", Vec::new())
        } else {
            let names = |category: u16| {
                query
                    .split('&')
                    .filter_map(|kv| kv.strip_prefix("categories="))
                    .flat_map(|cats| cats.split('x'))
                    .any(|cat| cat == category.to_string())
            };
            let first_page = query.split('&').any(|kv| kv == "page=0");
            let mut content = String::new();
            let mut listed = 0u32;
            if first_page && served.in_catalog && names(COLOR_SCHEME_CATEGORY) {
                content.push_str(entry);
                listed += 1;
            }
            if first_page
                && let Some((type_id, widget)) = &served.widget
                && names(*type_id)
            {
                content.push_str(widget);
                listed += 1;
            }
            for extra in &served.extras {
                if first_page && names(extra.type_id) {
                    content.push_str(&extra.xml);
                    listed += 1;
                }
            }
            let total = match served.total_items {
                TotalItems::Items(items) => Some(items),
                TotalItems::Default => Some(listed),
            };
            if first_page && served.catalog_garbage {
                ("200 OK", b"<html>maintenance</html>".to_vec())
            } else {
                ("200 OK", ocs(total, &content).into_bytes())
            }
        }
    } else {
        ("404 Not Found", Vec::new())
    };
    let location_header = location
        .map(|url| format!("Location: {url}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{location_header}\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn ocs(total: Option<u32>, content: &str) -> String {
    let total_items = total
        .map(|items| format!("<totalitems>{items}</totalitems>"))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>100</statuscode>\
         <message></message>{total_items}<itemsperpage>100</itemsperpage>\
         </meta><data>{content}</data></ocs>"
    )
}

/// The OCS `<content>` entry extra entry `entry` is served as, downloading
/// from the archive [`files_archive`] builds for it.
fn extra_entry_xml(host: &str, entry: &ExtraEntry) -> String {
    format!(
        "<content details=\"full\"><id>{id}</id><name>{name}</name>\
         <version>{version}</version><typeid>{type_id}</typeid>\
         <changed>2026-10-01T00:00:00+00:00</changed>\
         <downloadlink1>{host}/dl/extra-{id}.tar.gz</downloadlink1>\
         <download_version1>{version}</download_version1></content>",
        id = entry.id,
        name = entry.name,
        version = entry.version,
        type_id = entry.type_id,
    )
}

/// A `.tar.gz` holding `files`, each at its relative path, built with the
/// system `tar`.
fn files_archive(files: &[(&str, &str)]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let mut roots = std::collections::BTreeSet::new();
    for (relative, content) in files {
        let path = src.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        if let Some(root) = relative.split('/').next() {
            roots.insert(root.to_string());
        }
    }
    let archive = dir.path().join("extra.tar.gz");
    let mut command = Command::new("tar");
    command
        .args(["--create", "--gzip", "--file"])
        .arg(&archive)
        .arg("--directory")
        .arg(&src);
    for root in &roots {
        command.arg(root);
    }
    let status = command.status().unwrap();
    assert!(status.success(), "tar failed building the fixture archive");
    std::fs::read(archive).unwrap()
}

/// A `.tar.gz` holding the new color scheme file, built with the system `tar`.
fn scheme_archive() -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join(SCHEME_FILE), NEW_CONTENT).unwrap();
    let archive = dir.path().join("fake-scheme.tar.gz");
    let status = Command::new("tar")
        .arg("--create")
        .arg("--gzip")
        .arg("--file")
        .arg(&archive)
        .arg("--directory")
        .arg(&src)
        .arg(SCHEME_FILE)
        .status()
        .unwrap();
    assert!(status.success(), "tar failed building the fixture archive");
    std::fs::read(archive).unwrap()
}

/// A `.tar.gz` holding a Plasma widget package directory `<id>/` whose
/// `metadata.json` carries `id`, `name` and `version`, built with the system
/// `tar`. With `symlinked_metadata`, that file is a link out of the package
/// beside a regular `metadata.desktop`. With `escaping_links`, `contents/ui`
/// holds a chain of links that lands outside the package.
fn widget_archive(
    id: &str,
    name: &str,
    version: &str,
    symlinked_metadata: bool,
    escaping_links: bool,
) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("src").join(id);
    std::fs::create_dir_all(package.join("contents/ui")).unwrap();
    std::fs::write(
        package.join("metadata.json"),
        format!(
            r#"{{"KPackageStructure": "Plasma/Applet", "KPlugin": {{"Id": "{id}", "Name": "{name}", "Version": "{version}"}}}}"#
        ),
    )
    .unwrap();
    if symlinked_metadata {
        let metadata = package.join("metadata.json");
        std::fs::remove_file(&metadata).unwrap();
        std::os::unix::fs::symlink("../outside.json", &metadata).unwrap();
        std::fs::write(
            package.join("metadata.desktop"),
            format!("[Desktop Entry]\nX-KDE-PluginInfo-Name={id}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        package.join("contents/ui/main.qml"),
        "import QtQuick\nItem {}\n",
    )
    .unwrap();
    if escaping_links {
        // `s` names the package root; `l` walks through it and three more
        // levels up, so it stays inside only when the target is read as text.
        std::os::unix::fs::symlink("../..", package.join("contents/ui/s")).unwrap();
        std::os::unix::fs::symlink("s/../../..", package.join("contents/ui/l")).unwrap();
    }
    let archive = dir.path().join("fake-widget.tar.gz");
    let status = Command::new("tar")
        .arg("--create")
        .arg("--gzip")
        .arg("--file")
        .arg(&archive)
        .arg("--directory")
        .arg(dir.path().join("src"))
        .arg(id)
        .status()
        .unwrap();
    assert!(status.success(), "tar failed building the widget archive");
    std::fs::read(archive).unwrap()
}

/// Asserts `path` still holds exactly `expected`.
pub fn assert_content(path: &Path, expected: &str) {
    assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
}
