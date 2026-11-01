//! Harness for the public-API tests: a sandboxed child process per test and a
//! local fake KDE Store.
//!
//! The library resolves every path through `HOME` and `XDG_*`, and setting
//! environment variables in-process needs `unsafe`, which the workspace forbids.
//! So each test runs twice: the parent builds the fixture and the fake store,
//! then re-runs its own test binary filtered to that one test with the sandbox
//! environment set; the child calls the library and asserts on its results.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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

    /// Re-runs this test binary filtered to `test` inside the sandbox, and
    /// panics with the child's output unless it ran the test body to the end.
    pub fn run_child(&self, test: &str, store_url: &str) {
        let root = self.root.path();
        let marker = root.join("child-done");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root.join("tmp"))
            .env("PLASMOID_UPDATER_CONFIG_DIR", root.join("pu-config"))
            .env(CHILD_FLAG, "1")
            .env(STORE_URL_VAR, store_url)
            .env(MARKER_VAR, &marker)
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
        }
    }
}

/// A local HTTP server speaking enough of the KDE Store's OCS v1 API for the
/// library: the category listing, the per-id content lookup, and the download.
pub struct FakeStore {
    base: String,
}

impl FakeStore {
    pub fn start(listing: Listing) -> Self {
        let archive = scheme_archive();
        let md5 = listing
            .md5_override
            .map_or_else(|| format!("{:x}", md5::compute(&archive)), str::to_string);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = format!("http://127.0.0.1:{port}");
        let entry = format!(
            "<content details=\"full\"><id>{SCHEME_ID}</id><name>{SCHEME_NAME}</name>\
             <version>{version}</version><typeid>{COLOR_SCHEME_CATEGORY}</typeid>\
             <changed>2026-10-01T00:00:00+00:00</changed>\
             <downloadlink1>{host}/dl/fake-scheme.tar.gz</downloadlink1>\
             <download_version1>{version}</download_version1>\
             <downloadmd5sum1>{md5}</downloadmd5sum1>\
             <downloadsize1>{size_kb}</downloadsize1></content>",
            version = listing.version,
            size_kb = archive.len().div_ceil(1024),
        );
        let state = Arc::new(Served {
            entry,
            archive,
            in_catalog: listing.in_catalog,
            per_id_fails: listing.per_id_fails,
            per_id_fails_left: AtomicUsize::new(listing.per_id_fails_first),
            per_id_drops: listing.per_id_drops,
        });
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&state);
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        Self {
            base: format!("{host}/ocs/v1"),
        }
    }

    /// The OCS base URL to hand to `Config::with_store_url`.
    pub fn url(&self) -> &str {
        &self.base
    }
}

/// A store URL nothing listens on.
pub fn dead_store_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/ocs/v1")
}

struct Served {
    entry: String,
    archive: Vec<u8>,
    in_catalog: bool,
    per_id_fails: bool,
    per_id_fails_left: AtomicUsize,
    per_id_drops: bool,
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
    let (status, body): (&str, Vec<u8>) = if target.starts_with("/dl/") {
        ("200 OK", served.archive.clone())
    } else if target == format!("/ocs/v1/content/data/{SCHEME_ID}") {
        let fail_now = served
            .per_id_fails_left
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if served.per_id_drops {
            return;
        } else if served.per_id_fails || fail_now {
            ("503 Service Unavailable", Vec::new())
        } else {
            ("200 OK", ocs(1, entry).into_bytes())
        }
    } else if let Some(query) = target.strip_prefix("/ocs/v1/content/data?") {
        let first_page = query.split('&').any(|kv| kv == "page=0");
        let lists_schemes = query
            .split('&')
            .filter_map(|kv| kv.strip_prefix("categories="))
            .flat_map(|cats| cats.split('x'))
            .any(|cat| cat == COLOR_SCHEME_CATEGORY.to_string());
        if first_page && lists_schemes && served.in_catalog {
            ("200 OK", ocs(1, entry).into_bytes())
        } else {
            ("200 OK", ocs(0, "").into_bytes())
        }
    } else {
        ("404 Not Found", Vec::new())
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn ocs(total: u32, content: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>100</statuscode>\
         <message></message><totalitems>{total}</totalitems><itemsperpage>100</itemsperpage>\
         </meta><data>{content}</data></ocs>"
    )
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

/// Asserts `path` still holds exactly `expected`.
pub fn assert_content(path: &Path, expected: &str) {
    assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
}
