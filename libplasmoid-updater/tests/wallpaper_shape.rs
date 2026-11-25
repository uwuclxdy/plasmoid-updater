//! A wallpaper installed as a directory whose store entry now serves one
//! image: the directory keeps its path and its metadata and holds the new
//! image, and a wallpaper installed as one file keeps today's install. Each
//! test runs in its own sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use common::{NEW_VERSION, OLD_VERSION, Sandbox};
use libplasmoid_updater::{
    ComponentType, Config, FailureKind, Reporter, UpdateResult, check, get_installed, update,
};
use log::{LevelFilter, Log, Metadata, Record};

struct Quiet;

impl Reporter for Quiet {}

const WALLPAPER_ID: u64 = 999_301;
const WALLPAPER_NAME: &str = "Fake Forest";
const WALLPAPER_DIR: &str = "FakeForest";
/// `KDE Wallpaper (other)`.
const WALLPAPER_CATEGORY: u16 = 299;
const OLD_IMAGE: &str = "old-image";
const OLD_SCREENSHOT: &str = "old-screenshot";
/// A 2x1 RGB PNG (`magick identify`: `PNG 2x1`).
const PNG_2X1: &[u8] = b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x02\x00\x00\x00\x01\x08\x02\x00\x00\x00\x7b\x40\xe8\xdd\x00\x00\x00\x0d\x49\x44\x41\x54\x78\x9c\x63\x68\x70\x50\x00\x22\x00\x06\xe7\x01\xc1\x53\x8c\x54\x83\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82";
/// A 3x2 grayscale baseline JPEG (`magick identify`: `JPEG 3x2`).
const JPEG_3X2: &[u8] = b"\xff\xd8\xff\xe0\x00\x10\x4a\x46\x49\x46\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xdb\x00\x43\
\x00\x50\x37\x3c\x46\x3c\x32\x50\x46\x41\x46\x5a\x55\x50\x5f\x78\xc8\x82\x78\x6e\x6e\x78\xf5\xaf\
\xb9\x91\xc8\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\
\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xc0\x00\x0b\x08\x00\x02\
\x00\x03\x01\x01\x11\x00\xff\xc4\x00\x14\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
\x00\x00\x00\x00\xff\xc4\x00\x14\x10\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
\x00\x00\xff\xda\x00\x08\x01\x01\x00\x00\x3f\x00\x3f\xff\xd9";
/// A 5x4 lossless WebP (`magick identify`: `WEBP 5x4`).
const WEBP_5X4: &[u8] = b"\x52\x49\x46\x46\x1e\x00\x00\x00\x57\x45\x42\x50\x56\x50\x38\x4c\x11\x00\x00\x00\x2f\x04\xc0\x00\x00\x07\xd0\xcc\x9a\x75\xa6\xff\x81\x88\xe8\x7f\x00\x00";
const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"2\" height=\"1\"><rect width=\"2\" height=\"1\" fill=\"#0a0\"/></svg>\n";
/// A PNG cut after its signature: no header to read a size from.
const CUT_PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const DESKTOP: &str = "[Desktop Entry]\nName=Fake Forest\nX-KDE-PluginInfo-Name=FakeForest\nX-KDE-PluginInfo-Version=1.0\n";
const RECORD_SUFFIX: &str = ".plasmoid-updater-backup";

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

// --- log capture in the child ---

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl Log for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        LINES
            .lock()
            .unwrap()
            .push(format!("{}: {}", record.target(), record.args()));
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture;

/// The line the child logs once its capture is installed: a capture that
/// recorded nothing cannot pass a check for lines it must not hold.
const CAPTURE_ON: &str = "test: capture on";

/// Installs the child's log capture and logs [`CAPTURE_ON`] through it.
fn start_capture() {
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(LevelFilter::Trace);
    log::debug!(target: "test", "capture on");
}

/// Asserts the child's capture recorded [`CAPTURE_ON`], and no metadata patch
/// warning: the rebuilt folder kept no metadata to patch.
fn assert_capture_holds_no_metadata_patch_warning() {
    let lines = LINES.lock().unwrap();
    assert!(
        lines.iter().any(|line| line == CAPTURE_ON),
        "the capture recorded no line of the update: {lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("patch: failed to update installed metadata: ")),
        "a patch warned about a metadata file the rebuild did not keep: {lines:#?}"
    );
}

// --- the fake store ---

/// A store answering the wallpaper's per-id lookup with an entry whose one
/// link serves `body` as `file`, with its MD5, and an empty catalog page
/// otherwise.
fn start_store(file: &'static str, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let entry = format!(
        "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>100</statuscode>\
         <message></message><totalitems>1</totalitems><itemsperpage>100</itemsperpage></meta>\
         <data><content details=\"full\"><id>{WALLPAPER_ID}</id><name>{WALLPAPER_NAME}</name>\
         <version>{NEW_VERSION}</version><typeid>{WALLPAPER_CATEGORY}</typeid>\
         <changed>2026-10-01T00:00:00+00:00</changed>\
         <downloadlink1>{host}/dl/{file}</downloadlink1>\
         <download_version1>{NEW_VERSION}</download_version1>\
         <downloadmd5sum1>{md5:x}</downloadmd5sum1></content></data></ocs>",
        md5 = md5::compute(&body),
    );
    let empty = "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>100</statuscode>\
                 <message></message><totalitems>0</totalitems><itemsperpage>100</itemsperpage></meta>\
                 <data></data></ocs>";
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                header.clear();
            }
            let target = request.split_whitespace().nth(1).unwrap_or("").to_string();
            let answer: &[u8] = if target == format!("/dl/{file}") {
                &body
            } else if target == format!("/ocs/v1/content/data/{WALLPAPER_ID}") {
                entry.as_bytes()
            } else {
                empty.as_bytes()
            };
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                answer.len()
            )
            .into_bytes();
            response.extend_from_slice(answer);
            if let Err(e) = stream.write_all(&response) {
                eprintln!("fake store: answering {target}: {e}");
            }
        }
    });
    format!("{host}/ocs/v1")
}

/// A `.tar.gz` holding `files`, each at its relative path, built with the
/// system `tar`.
fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let mut roots = std::collections::BTreeSet::new();
    for (relative, content) in files {
        let path = src.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        roots.insert(relative.split('/').next().unwrap().to_string());
    }
    let archive = dir.path().join("served.tar.gz");
    let status = Command::new("tar")
        .args(["--create", "--gzip", "--file"])
        .arg(&archive)
        .arg("--directory")
        .arg(&src)
        .args(&roots)
        .status()
        .unwrap();
    assert!(status.success(), "tar failed building the fixture archive");
    std::fs::read(archive).unwrap()
}

// --- fixtures ---

fn wallpapers(sandbox: &Sandbox) -> PathBuf {
    sandbox.data_home().join("wallpapers")
}

fn wallpaper_dir(sandbox: &Sandbox) -> PathBuf {
    wallpapers(sandbox).join(WALLPAPER_DIR)
}

/// Registers the wallpaper at `installed_file`, as KNewStuff records it.
fn register(sandbox: &Sandbox, installed_file: &str) {
    common::write_registry(
        sandbox,
        "wallpaper.knsregistry",
        WALLPAPER_CATEGORY,
        WALLPAPER_NAME,
        OLD_VERSION,
        installed_file,
        Some(WALLPAPER_ID),
    );
}

/// A wallpaper package directory at 1.0: `metadata.json`, one image and a
/// screenshot, registered as KNewStuff records an unpacked archive (`<dir>/*`).
fn install_package(sandbox: &Sandbox) -> PathBuf {
    let dir = wallpaper_dir(sandbox);
    std::fs::create_dir_all(dir.join("contents/images")).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        format!(
            r#"{{"KPlugin":{{"Id":"{WALLPAPER_DIR}","Name":"{WALLPAPER_NAME}","Version":"{OLD_VERSION}"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("contents/images/1920x1080.png"), OLD_IMAGE).unwrap();
    std::fs::write(dir.join("contents/screenshot.png"), OLD_SCREENSHOT).unwrap();
    register(sandbox, &format!("{}/*", dir.display()));
    dir
}

/// Every entry under `dir`, relative, a directory with a trailing `/`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            let shown = path.strip_prefix(dir).unwrap().display().to_string();
            if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                out.push(format!("{shown}/"));
                stack.push(path);
            } else {
                out.push(shown);
            }
        }
    }
    out.sort();
    out
}

/// Every file under `dir` with its bytes, keyed by its relative path.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    entries(dir)
        .into_iter()
        .filter(|entry| !entry.ends_with('/'))
        .map(|entry| {
            let bytes = std::fs::read(dir.join(&entry)).unwrap();
            (entry, bytes)
        })
        .collect()
}

fn registry(sandbox: &Sandbox) -> String {
    std::fs::read_to_string(sandbox.data_home().join("knewstuff3/wallpaper.knsregistry")).unwrap()
}

/// The text of every `<tag>` element in `xml`, in order.
fn elements(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    xml.split(open.as_str())
        .skip(1)
        .map(|rest| rest.split(close.as_str()).next().unwrap().to_string())
        .collect()
}

/// The registry holds one wallpaper entry at `NEW_VERSION` whose one
/// `installedfile` names `path`, in one of the spellings KNewStuff and the
/// updater record a component by.
fn assert_registry_names(sandbox: &Sandbox, path: &Path) {
    let registry = registry(sandbox);
    assert_eq!(
        registry.matches("<stuff ").count(),
        1,
        "not one entry: {registry}"
    );
    assert_eq!(
        elements(&registry, "version"),
        [NEW_VERSION],
        "the entry's version: {registry}"
    );
    let installed = elements(&registry, "installedfile");
    let named = [
        path.display().to_string(),
        format!("{}/metadata.json", path.display()),
        format!("{}/*", path.display()),
    ];
    assert!(
        installed.len() == 1 && named.contains(&installed[0]),
        "the entry does not name {}: {registry}",
        path.display()
    );
}

/// Every path under the run's backups dir named `name`.
fn backups_named(sandbox: &Sandbox, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![sandbox.cache_dir().join("plasmoid-updater/backups")];
    while let Some(dir) = pending.pop() {
        let Ok(listing) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .is_some_and(|entry_name| entry_name == name)
            {
                found.push(path.clone());
            }
            if path.is_dir() {
                pending.push(path);
            }
        }
    }
    found
}

fn record_beside(component: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(component.file_name().unwrap());
    name.push(RECORD_SUFFIX);
    component.with_file_name(name)
}

/// The wallpaper path the child's sandbox holds, as discovery spells it.
fn child_wallpaper(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap())
        .join("wallpapers")
        .join(name)
}

/// The child's run for a wallpaper at `name` that updates: the check offers
/// it, the update lands it verified at the scanned path, and a second check
/// offers nothing while discovery still lists it at the new version. `also`
/// runs on the child's own legs before it marks the body complete, so its
/// failure names a body that did not finish.
fn updates_in_place_checking(child: common::Child, name: &str, also: impl FnOnce()) {
    let config = config(&child.store_url);
    let found = check(&config, &()).unwrap();
    assert_eq!(found.available_updates.len(), 1, "{found:?}");
    let scanned = &found.available_updates[0].installed;
    assert_eq!(scanned.component_type, ComponentType::Wallpaper);
    assert_eq!(scanned.path, child_wallpaper(name), "{found:?}");

    let result = update(&config, &Quiet).unwrap();
    assert!(result.failed.is_empty(), "the update failed: {result:?}");
    assert_eq!(result.succeeded.len(), 1, "{result:?}");
    assert_eq!(result.succeeded[0].path, scanned.path, "{result:?}");
    assert_eq!(result.succeeded[0].version, NEW_VERSION, "{result:?}");

    let again = check(&config, &()).unwrap();
    assert!(
        again.available_updates.is_empty()
            && again.diagnostics.is_empty()
            && again.skipped.is_empty(),
        "the second check: {again:?}"
    );
    let installed = get_installed(&config).unwrap();
    assert_eq!(installed.len(), 1, "{installed:?}");
    assert_eq!(installed[0].path, scanned.path, "{installed:?}");
    assert_eq!(installed[0].version, NEW_VERSION, "{installed:?}");
    also();
    child.done();
}

/// The child's run for a wallpaper at `name` that updates in place.
fn updates_in_place(child: common::Child, name: &str) {
    updates_in_place_checking(child, name, || {});
}

/// The child's run for an update that fails the wallpaper: one failure of
/// `kind` with exactly `message`.
fn fails_with(child: common::Child, kind: FailureKind, message: &str) {
    let result: UpdateResult = update(&config(&child.store_url), &Quiet).unwrap();
    assert!(result.succeeded.is_empty(), "{result:?}");
    assert_eq!(result.failed.len(), 1, "{result:?}");
    assert_eq!(result.failed[0].kind, kind, "{result:?}");
    assert_eq!(result.failed[0].message, message, "{result:?}");
    child.done();
}

/// The package directory after an update from `file`: exactly its patched
/// `metadata.json` plus `image`, holding `body`.
fn assert_rebuilt_package(sandbox: &Sandbox, image: &str, body: &[u8]) {
    let dir = wallpaper_dir(sandbox);
    assert_eq!(
        entries(&dir),
        [
            "contents/".to_string(),
            "contents/images/".to_string(),
            image.to_string(),
            "metadata.json".to_string(),
        ],
        "the rebuilt directory"
    );
    assert_eq!(
        std::fs::read(dir.join(image)).unwrap(),
        body,
        "the image is not the download"
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(
        metadata,
        serde_json::json!({
            "KPlugin": {"Id": WALLPAPER_DIR, "Name": WALLPAPER_NAME, "Version": NEW_VERSION}
        }),
        "the kept metadata.json"
    );
}

/// `test`'s child, run under `strace` with `inject` applied to the first
/// `renameat2` naming `path`, or `None` on a host without `strace`.
fn under_strace(
    sandbox: &Sandbox,
    test: &str,
    store_url: &str,
    path: &Path,
    inject: &str,
) -> Option<std::process::Output> {
    if Command::new("strace").arg("-V").output().is_err() {
        eprintln!("skipped: no strace on this host to inject the fault with");
        return None;
    }
    let inner = sandbox.child_command(test, store_url);
    let mut command = sandbox.command("strace");
    command
        .args(["-f", "-o", "/dev/null", "-P"])
        .arg(path)
        .args(["-e", "trace=renameat2", "-e"])
        .arg(format!("inject=renameat2:{inject}:when=1"))
        .arg(inner.get_program())
        .args(inner.get_args());
    for (key, value) in inner.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    Some(command.output().unwrap())
}

// --- the directory wallpaper served one image ---

#[test]
fn a_directory_wallpaper_updates_from_a_raw_image() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    sandbox.run_child(
        "a_directory_wallpaper_updates_from_a_raw_image",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert!(dir.is_dir(), "{} is no longer a directory", dir.display());
    assert_rebuilt_package(&sandbox, "contents/images/2x1.png", PNG_2X1);
    assert_registry_names(&sandbox, &dir);
    assert!(
        !record_beside(&dir).exists(),
        "an install record stayed behind"
    );
}

#[test]
fn a_directory_wallpaper_updates_from_an_archive_holding_one_loose_image() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "a_directory_wallpaper_updates_from_an_archive_holding_one_loose_image",
        &start_store("forest.tar.gz", tar_gz(&[("forest.png", PNG_2X1)])),
    );
    assert_rebuilt_package(&sandbox, "contents/images/2x1.png", PNG_2X1);
}

/// The rebuilt-directory arm takes an archive holding one loose image and
/// nothing else: a second image in the same archive is refused by the
/// plugin-id check, the installed directory untouched.
#[test]
fn a_two_image_archive_is_refused_as_another_widget() {
    const MESSAGE: &str = "store entry is a different widget: installed \"FakeForest\", the package records no plugin id";
    if let Some(child) = common::child() {
        fails_with(child, FailureKind::Install, MESSAGE);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    let before = snapshot(&dir);
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "a_two_image_archive_is_refused_as_another_widget",
        &start_store(
            "forest.tar.gz",
            tar_gz(&[("forest.png", PNG_2X1), ("beach.png", PNG_2X1)]),
        ),
    );
    assert_eq!(
        std::fs::metadata(&dir).unwrap().ino(),
        inode,
        "the directory was replaced"
    );
    assert_eq!(snapshot(&dir), before, "the directory changed");
    let backups = backups_named(&sandbox, WALLPAPER_DIR);
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// An image beside any other entry is no single-image archive either: the
/// pack is refused the same way.
#[test]
fn an_archive_holding_an_image_and_a_text_file_is_refused_as_another_widget() {
    const MESSAGE: &str = "store entry is a different widget: installed \"FakeForest\", the package records no plugin id";
    if let Some(child) = common::child() {
        fails_with(child, FailureKind::Install, MESSAGE);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    let before = snapshot(&dir);
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "an_archive_holding_an_image_and_a_text_file_is_refused_as_another_widget",
        &start_store(
            "forest.tar.gz",
            tar_gz(&[("forest.png", PNG_2X1), ("README", b"a wallpaper pack\n")]),
        ),
    );
    assert_eq!(
        std::fs::metadata(&dir).unwrap().ino(),
        inode,
        "the directory was replaced"
    );
    assert_eq!(snapshot(&dir), before, "the directory changed");
    let backups = backups_named(&sandbox, WALLPAPER_DIR);
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// The plugin-id exemption covers only a directory wallpaper whose archive
/// holds one loose image: another widget's package is still refused.
#[test]
fn a_directory_wallpaper_served_another_widgets_package_is_refused() {
    const MESSAGE: &str =
        "store entry is a different widget: installed \"FakeForest\", package \"OtherWidget\"";
    if let Some(child) = common::child() {
        fails_with(child, FailureKind::Install, MESSAGE);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    let before = snapshot(&dir);
    let metadata = r#"{"KPlugin":{"Id":"OtherWidget","Name":"Other","Version":"2.0"}}"#;
    sandbox.run_child(
        "a_directory_wallpaper_served_another_widgets_package_is_refused",
        &start_store(
            "other.tar.gz",
            tar_gz(&[
                ("Other/metadata.json", metadata.as_bytes()),
                ("Other/contents/images/1920x1080.png", PNG_2X1),
            ]),
        ),
    );
    assert_eq!(snapshot(&dir), before, "the directory changed");
    let backups = backups_named(&sandbox, WALLPAPER_DIR);
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

#[test]
fn a_desktop_only_package_keeps_its_metadata_desktop_byte_for_byte() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpaper_dir(&sandbox);
    std::fs::create_dir_all(dir.join("contents/images")).unwrap();
    std::fs::write(dir.join("metadata.desktop"), DESKTOP).unwrap();
    std::fs::write(dir.join("contents/images/1920x1080.png"), OLD_IMAGE).unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_desktop_only_package_keeps_its_metadata_desktop_byte_for_byte",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([
            ("contents/images/2x1.png".to_string(), PNG_2X1.to_vec()),
            ("metadata.desktop".to_string(), DESKTOP.as_bytes().to_vec()),
        ]),
        "the rebuilt directory"
    );
    assert_eq!(
        entries(&dir),
        [
            "contents/",
            "contents/images/",
            "contents/images/2x1.png",
            "metadata.desktop"
        ]
    );
    assert_registry_names(&sandbox, &dir);
}

/// A package directory holding `metadata.json` and no `contents/` is still a
/// package: the image goes under `contents/images/`, not beside the metadata.
#[test]
fn a_metadata_only_package_gets_the_image_under_contents_images() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpaper_dir(&sandbox);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        format!(
            r#"{{"KPlugin":{{"Id":"{WALLPAPER_DIR}","Name":"{WALLPAPER_NAME}","Version":"{OLD_VERSION}"}}}}"#
        ),
    )
    .unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_metadata_only_package_gets_the_image_under_contents_images",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/2x1.png", PNG_2X1);
    assert_registry_names(&sandbox, &dir);
}

/// The same for a package holding only `metadata.desktop` and no `contents/`:
/// the image goes under `contents/images/` and the desktop file stays.
#[test]
fn a_desktop_only_package_without_contents_keeps_its_desktop_file() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpaper_dir(&sandbox);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("metadata.desktop"), DESKTOP).unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_desktop_only_package_without_contents_keeps_its_desktop_file",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([
            ("contents/images/2x1.png".to_string(), PNG_2X1.to_vec()),
            ("metadata.desktop".to_string(), DESKTOP.as_bytes().to_vec()),
        ]),
        "the rebuilt directory"
    );
    assert_eq!(
        entries(&dir),
        [
            "contents/",
            "contents/images/",
            "contents/images/2x1.png",
            "metadata.desktop"
        ]
    );
    assert_registry_names(&sandbox, &dir);
}

#[test]
fn a_plain_folder_of_images_ends_holding_only_the_new_image() {
    if let Some(child) = common::child() {
        start_capture();
        updates_in_place_checking(
            child,
            "Sweet",
            assert_capture_holds_no_metadata_patch_warning,
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    std::fs::write(dir.join("b.png"), "old-b").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_plain_folder_of_images_ends_holding_only_the_new_image",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("forest.png".to_string(), PNG_2X1.to_vec())]),
        "the plain folder"
    );
    assert_eq!(entries(&dir), ["forest.png"]);
    assert_registry_names(&sandbox, &dir);
}

/// A plain folder takes the served image as is, so a raw file the store serves
/// as `metadata.json` lands as that image: its bytes stay exactly as served,
/// and the run warns about no metadata patch.
#[test]
fn a_plain_folder_served_a_metadata_json_image_keeps_the_served_bytes() {
    if let Some(child) = common::child() {
        start_capture();
        updates_in_place_checking(
            child,
            "Sweet",
            assert_capture_holds_no_metadata_patch_warning,
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_plain_folder_served_a_metadata_json_image_keeps_the_served_bytes",
        &start_store("metadata.json", PNG_2X1.to_vec()),
    );
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("metadata.json".to_string(), PNG_2X1.to_vec())]),
        "the plain folder"
    );
    assert_registry_names(&sandbox, &dir);
}

/// A plain folder keeps a served JSON document named `metadata.json` byte for
/// byte: the folder kept no installed metadata, so there is nothing to patch.
#[test]
fn a_plain_folder_served_a_metadata_json_document_keeps_the_served_bytes() {
    const SERVED: &[u8] = br#"{"KPlugin":{"Version":"1.0"}}"#;
    if let Some(child) = common::child() {
        start_capture();
        updates_in_place_checking(
            child,
            "Sweet",
            assert_capture_holds_no_metadata_patch_warning,
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_plain_folder_served_a_metadata_json_document_keeps_the_served_bytes",
        &start_store("metadata.json", SERVED.to_vec()),
    );
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("metadata.json".to_string(), SERVED.to_vec())]),
        "the plain folder"
    );
    assert_registry_names(&sandbox, &dir);
}

#[test]
fn a_jpeg_lands_named_by_its_pixel_size() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "a_jpeg_lands_named_by_its_pixel_size",
        &start_store("forest.jpeg", JPEG_3X2.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/3x2.jpg", JPEG_3X2);
}

#[test]
fn a_webp_lands_named_by_its_pixel_size() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "a_webp_lands_named_by_its_pixel_size",
        &start_store("forest.webp", WEBP_5X4.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/5x4.webp", WEBP_5X4);
}

#[test]
fn an_svg_lands_under_its_download_file_name() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "an_svg_lands_under_its_download_file_name",
        &start_store("forest.svg", SVG.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/forest.svg", SVG);
}

/// The SVG test is case-insensitive: an uppercase extension still lands under
/// its own name instead of being read for a pixel size.
#[test]
fn an_uppercase_svg_extension_still_lands_under_its_file_name() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "an_uppercase_svg_extension_still_lands_under_its_file_name",
        &start_store("forest.SVG", SVG.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/forest.SVG", SVG);
}

/// A `?query` on an SVG's link does not hide its format: a package served
/// `forest.svg?dl=1` holds `forest.svg` under `contents/images/`.
#[test]
fn an_svg_download_with_a_query_lands_under_its_file_name() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "an_svg_download_with_a_query_lands_under_its_file_name",
        &start_store("forest.svg?dl=1", SVG.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/forest.svg", SVG);
}

/// The exchange that swaps the rebuilt directory in fails (`EIO` injected on
/// it): the backup is put back and the install record goes.
#[test]
fn a_failed_exchange_restores_the_directory_and_leaves_no_record() {
    const TEST: &str = "a_failed_exchange_restores_the_directory_and_leaves_no_record";
    if let Some(child) = common::child() {
        let dir = child_wallpaper(WALLPAPER_DIR);
        fails_with(
            child,
            FailureKind::Install,
            &format!("{}: Input/output error (os error 5)", dir.display()),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    let before = snapshot(&dir);
    let store = start_store("forest.png", PNG_2X1.to_vec());
    let Some(output) = under_strace(&sandbox, TEST, &store, &dir, "error=EIO") else {
        return;
    };
    assert!(
        output.status.success() && sandbox.child_marker().exists(),
        "child failed ({})\n--- stderr\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(entries(&dir), entries_of(&before), "the restored tree");
    assert_eq!(snapshot(&dir), before, "the restored bytes");
    assert!(
        !record_beside(&dir).exists(),
        "an install record stayed behind"
    );
    assert_eq!(
        backups_named(&sandbox, WALLPAPER_DIR).len(),
        1,
        "the exchange failed after the backup, so exactly one backup is expected"
    );
}

/// The directories a snapshot's files imply, plus the files, sorted the way
/// [`entries`] lists them.
fn entries_of(files: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let mut out: Vec<String> = files.keys().cloned().collect();
    for file in files.keys() {
        let mut parent = Path::new(file).parent();
        while let Some(dir) = parent.filter(|dir| !dir.as_os_str().is_empty()) {
            out.push(format!("{}/", dir.display()));
            parent = dir.parent();
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A run killed at the exchange (after its backup and its install record) is
/// repaired by the next `update` before that run installs, and the next run
/// then updates the wallpaper in place.
#[test]
fn a_run_killed_before_the_swap_is_repaired_before_the_next_install() {
    const TEST: &str = "a_run_killed_before_the_swap_is_repaired_before_the_next_install";
    if let Some(child) = common::child() {
        log::set_logger(&CAPTURE).unwrap();
        log::set_max_level(LevelFilter::Trace);
        let dir = child_wallpaper(WALLPAPER_DIR);
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert!(result.failed.is_empty(), "the update failed: {result:?}");
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        let lines = LINES.lock().unwrap().clone();
        let restored = format!("restore: restored {} from ", dir.display());
        let copied = format!("install: copied wallpaper dir to {}", dir.display());
        let restored_at = lines.iter().position(|line| line.starts_with(&restored));
        let copied_at = lines.iter().position(|line| *line == copied);
        assert!(
            matches!((restored_at, copied_at), (Some(r), Some(c)) if r < c),
            "no repair before the install: {lines:#?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    let before = snapshot(&dir);
    let store = start_store("forest.png", PNG_2X1.to_vec());
    let Some(killed) = under_strace(&sandbox, TEST, &store, &dir, "signal=SIGKILL") else {
        return;
    };
    assert!(
        !sandbox.child_marker().exists(),
        "the run was not killed\n--- stderr\n{}",
        String::from_utf8_lossy(&killed.stderr),
    );
    assert!(
        record_beside(&dir).exists(),
        "the killed run left no install record"
    );
    assert_eq!(
        snapshot(&dir),
        before,
        "the killed run changed the directory"
    );
    sandbox.run_child(TEST, &store);
    assert_rebuilt_package(&sandbox, "contents/images/2x1.png", PNG_2X1);
    assert!(
        !record_beside(&dir).exists(),
        "an install record stayed behind"
    );
}

#[test]
fn an_image_whose_size_cannot_be_read_fails_before_any_backup() {
    if let Some(child) = common::child() {
        fails_with(
            child,
            FailureKind::Install,
            "cannot read the pixel size of downloaded image \"forest.png\" to name it in the wallpaper folder (failed to fill whole buffer)",
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    let before = snapshot(&dir);
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "an_image_whose_size_cannot_be_read_fails_before_any_backup",
        &start_store("forest.png", CUT_PNG.to_vec()),
    );
    assert_eq!(
        std::fs::metadata(&dir).unwrap().ino(),
        inode,
        "the directory was replaced"
    );
    assert_eq!(snapshot(&dir), before, "the directory changed");
    let backups = backups_named(&sandbox, WALLPAPER_DIR);
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// A package directory with `contents/` but no metadata file gets the image
/// under `contents/images/` and no metadata the store never shipped.
#[test]
fn a_package_holding_only_contents_gets_no_invented_metadata() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpaper_dir(&sandbox);
    std::fs::create_dir_all(dir.join("contents/images")).unwrap();
    std::fs::write(dir.join("contents/images/1920x1080.png"), OLD_IMAGE).unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_package_holding_only_contents_gets_no_invented_metadata",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert_eq!(
        entries(&dir),
        ["contents/", "contents/images/", "contents/images/2x1.png"]
    );
    assert_eq!(
        std::fs::read(dir.join("contents/images/2x1.png")).unwrap(),
        PNG_2X1
    );
}

/// A package holding both metadata files keeps both: `metadata.json` with the
/// new version, though it is installed read-only, and `metadata.desktop` byte
/// for byte.
#[test]
fn a_package_with_both_metadata_files_keeps_both() {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    std::fs::write(dir.join("metadata.desktop"), DESKTOP).unwrap();
    std::fs::set_permissions(
        dir.join("metadata.json"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    sandbox.run_child(
        "a_package_with_both_metadata_files_keeps_both",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert_eq!(
        entries(&dir),
        [
            "contents/",
            "contents/images/",
            "contents/images/2x1.png",
            "metadata.desktop",
            "metadata.json"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("metadata.desktop")).unwrap(),
        DESKTOP
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(
        metadata,
        serde_json::json!({
            "KPlugin": {"Id": WALLPAPER_DIR, "Name": WALLPAPER_NAME, "Version": NEW_VERSION}
        })
    );
}

/// A kept `metadata.json` that does not parse lands as it was, with the
/// warning the metadata patch already prints; the registry carries the
/// version.
#[test]
fn an_unparsable_metadata_json_is_kept_as_is_with_a_warning() {
    const BROKEN: &str = "{ not json";
    if let Some(child) = common::child() {
        log::set_logger(&CAPTURE).unwrap();
        log::set_max_level(LevelFilter::Trace);
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert!(result.failed.is_empty(), "the update failed: {result:?}");
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        let lines = LINES.lock().unwrap().clone();
        let warned: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with("patch: failed to update installed metadata: "))
            .collect();
        assert_eq!(warned.len(), 1, "{lines:#?}");
        assert!(
            warned[0].contains("metadata.json") && warned[0].contains("failed to parse"),
            "{lines:#?}"
        );
        let installed = child_wallpaper(WALLPAPER_DIR).join("metadata.json");
        assert!(
            warned[0].contains(&installed.display().to_string()),
            "the warning names no installed file: {lines:#?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpaper_dir(&sandbox);
    std::fs::create_dir_all(dir.join("contents/images")).unwrap();
    std::fs::write(dir.join("metadata.json"), BROKEN).unwrap();
    std::fs::write(dir.join("contents/images/1920x1080.png"), OLD_IMAGE).unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "an_unparsable_metadata_json_is_kept_as_is_with_a_warning",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([
            ("contents/images/2x1.png".to_string(), PNG_2X1.to_vec()),
            ("metadata.json".to_string(), BROKEN.as_bytes().to_vec()),
        ])
    );
    assert_registry_names(&sandbox, &dir);
}

/// A download whose link names no file (it ends in `/`) leaves a plain
/// folder nothing to name the image by: the update fails before any backup.
#[test]
fn a_nameless_download_fails_a_plain_folder_before_any_backup() {
    if let Some(child) = common::child() {
        fails_with(
            child,
            FailureKind::Install,
            "downloaded image has no file name to put in the wallpaper folder (\"\")",
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "a_nameless_download_fails_a_plain_folder_before_any_backup",
        &start_store("", PNG_2X1.to_vec()),
    );
    assert_eq!(std::fs::metadata(&dir).unwrap().ino(), inode);
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("a.jpg".to_string(), b"old-a".to_vec())])
    );
    let backups = backups_named(&sandbox, "Sweet");
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// A package directory served an SVG whose link names no file fails with the
/// no-file-name text, not the pixel-size one: an SVG has no pixel size to name
/// it by, and a name that is no file name names nothing.
#[test]
fn a_nameless_svg_for_a_package_names_no_file() {
    if let Some(child) = common::child() {
        fails_with(
            child,
            FailureKind::Install,
            "downloaded image has no file name to put in the wallpaper folder (\"\")",
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = install_package(&sandbox);
    let before = snapshot(&dir);
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "a_nameless_svg_for_a_package_names_no_file",
        &start_store("", SVG.to_vec()),
    );
    assert_eq!(
        std::fs::metadata(&dir).unwrap().ino(),
        inode,
        "the directory was replaced"
    );
    assert_eq!(snapshot(&dir), before, "the directory changed");
    let backups = backups_named(&sandbox, WALLPAPER_DIR);
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// A package directory names a raster image by its pixel size, so a link that
/// names no file updates it: nothing on that arm reads the served name.
#[test]
fn a_nameless_download_updates_a_package_directory() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "a_nameless_download_updates_a_package_directory",
        &start_store("", PNG_2X1.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/2x1.png", PNG_2X1);
    assert_registry_names(&sandbox, &wallpaper_dir(&sandbox));
}

/// The same for a name whose escapes do not decode to UTF-8: the raster
/// package arm never puts it in a path.
#[test]
fn an_undecodable_download_name_updates_a_package_directory() {
    if let Some(child) = common::child() {
        updates_in_place(child, WALLPAPER_DIR);
        return;
    }
    let sandbox = Sandbox::new();
    install_package(&sandbox);
    sandbox.run_child(
        "an_undecodable_download_name_updates_a_package_directory",
        &start_store("%FF.png", PNG_2X1.to_vec()),
    );
    assert_rebuilt_package(&sandbox, "contents/images/2x1.png", PNG_2X1);
    assert_registry_names(&sandbox, &wallpaper_dir(&sandbox));
}

/// The download's URL names the image it lands as: `%20` decodes and the
/// `?query` goes, so a plain folder served `Forest%20Night.png?dl=1` ends
/// holding `Forest Night.png`.
#[test]
fn a_raw_download_is_named_by_its_decoded_url_file_name() {
    if let Some(child) = common::child() {
        updates_in_place(child, "Sweet");
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    sandbox.run_child(
        "a_raw_download_is_named_by_its_decoded_url_file_name",
        &start_store("Forest%20Night.png?dl=1", PNG_2X1.to_vec()),
    );
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("Forest Night.png".to_string(), PNG_2X1.to_vec())]),
        "the plain folder"
    );
    assert_eq!(entries(&dir), ["Forest Night.png"]);
    assert_registry_names(&sandbox, &dir);
}

/// A download name that decodes to a path is no file name: the update fails
/// the wallpaper before any backup, with the no-file-name text.
#[test]
fn a_download_name_that_decodes_to_a_path_fails_before_any_backup() {
    if let Some(child) = common::child() {
        fails_with(
            child,
            FailureKind::Install,
            "downloaded image has no file name to put in the wallpaper folder (\"a/b.png\")",
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "a_download_name_that_decodes_to_a_path_fails_before_any_backup",
        &start_store("a%2Fb.png", PNG_2X1.to_vec()),
    );
    assert_eq!(std::fs::metadata(&dir).unwrap().ino(), inode);
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("a.jpg".to_string(), b"old-a".to_vec())])
    );
    let backups = backups_named(&sandbox, "Sweet");
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// A download name that decodes to hold a NUL is no file name either: it
/// fails the wallpaper with the same no-file-name text, before any backup.
#[test]
fn a_download_name_holding_a_nul_fails_before_any_backup() {
    if let Some(child) = common::child() {
        fails_with(
            child,
            FailureKind::Install,
            "downloaded image has no file name to put in the wallpaper folder (\"a\\0b.png\")",
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "a_download_name_holding_a_nul_fails_before_any_backup",
        &start_store("a%00b.png", PNG_2X1.to_vec()),
    );
    assert_eq!(std::fs::metadata(&dir).unwrap().ino(), inode);
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("a.jpg".to_string(), b"old-a".to_vec())])
    );
    let backups = backups_named(&sandbox, "Sweet");
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// A download name whose `%xx` escapes do not decode to UTF-8 names no file:
/// a plain folder fails before any backup, naming it the way the store served
/// it.
#[test]
fn a_download_name_that_does_not_decode_to_utf8_fails_before_any_backup() {
    if let Some(child) = common::child() {
        fails_with(
            child,
            FailureKind::Install,
            "downloaded image has no file name to put in the wallpaper folder (\"%FF.png\")",
        );
        return;
    }
    let sandbox = Sandbox::new();
    let dir = wallpapers(&sandbox).join("Sweet");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.jpg"), "old-a").unwrap();
    register(&sandbox, &format!("{}/*", dir.display()));
    let inode = std::fs::metadata(&dir).unwrap().ino();
    sandbox.run_child(
        "a_download_name_that_does_not_decode_to_utf8_fails_before_any_backup",
        &start_store("%FF.png", PNG_2X1.to_vec()),
    );
    assert_eq!(std::fs::metadata(&dir).unwrap().ino(), inode);
    assert_eq!(
        snapshot(&dir),
        BTreeMap::from([("a.jpg".to_string(), b"old-a".to_vec())])
    );
    let backups = backups_named(&sandbox, "Sweet");
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

// --- the wallpaper installed as one file ---

#[test]
fn a_single_file_wallpaper_served_a_raw_image_stays_that_file() {
    if let Some(child) = common::child() {
        updates_in_place(child, "Nature.png");
        return;
    }
    let sandbox = Sandbox::new();
    let file = wallpapers(&sandbox).join("Nature.png");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, OLD_IMAGE).unwrap();
    register(&sandbox, &file.display().to_string());
    sandbox.run_child(
        "a_single_file_wallpaper_served_a_raw_image_stays_that_file",
        &start_store("forest.png", PNG_2X1.to_vec()),
    );
    assert!(
        std::fs::symlink_metadata(&file).unwrap().is_file(),
        "{} is no longer a file",
        file.display()
    );
    assert_eq!(std::fs::read(&file).unwrap(), PNG_2X1);
    assert_registry_names(&sandbox, &file);
}

/// The mirror case: a wallpaper installed as one file whose store entry now
/// ships a package becomes a directory at the same path.
#[test]
fn a_single_file_wallpaper_served_a_package_becomes_a_directory_at_its_path() {
    if let Some(child) = common::child() {
        updates_in_place(child, "Nature.jpg");
        return;
    }
    let sandbox = Sandbox::new();
    let file = wallpapers(&sandbox).join("Nature.jpg");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, OLD_IMAGE).unwrap();
    register(&sandbox, &file.display().to_string());
    let metadata = format!(
        r#"{{"KPlugin":{{"Id":"Nature","Name":"{WALLPAPER_NAME}","Version":"{NEW_VERSION}"}}}}"#
    );
    sandbox.run_child(
        "a_single_file_wallpaper_served_a_package_becomes_a_directory_at_its_path",
        &start_store(
            "nature.tar.gz",
            tar_gz(&[
                ("Nature/metadata.json", metadata.as_bytes()),
                ("Nature/contents/images/1920x1080.jpg", b"new-image"),
            ]),
        ),
    );
    assert_eq!(
        entries(&file),
        [
            "contents/",
            "contents/images/",
            "contents/images/1920x1080.jpg",
            "metadata.json"
        ],
        "the package at the wallpaper's path"
    );
    assert_eq!(
        std::fs::read(file.join("contents/images/1920x1080.jpg")).unwrap(),
        b"new-image"
    );
    let installed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(
        installed,
        serde_json::from_str::<serde_json::Value>(&metadata).unwrap(),
        "the package's metadata.json"
    );
    assert_registry_names(&sandbox, &file);
    assert!(
        !record_beside(&file).exists(),
        "an install record stayed behind"
    );
}
