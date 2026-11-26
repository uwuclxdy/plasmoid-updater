//! An update that fails before any step writes the component (its checksum,
//! its extraction, a package check, a lookup of what the type installs from
//! the unpacked tree) leaves the component as it was: no backup is taken and
//! none is restored over it. One that fails while the install writes it still
//! puts the backup back. Each test runs in its own sandboxed child process
//! (see `common`).

#![cfg(target_os = "linux")]
#![expect(clippy::unwrap_used, reason = "test helpers outside #[test] fns")]

mod common;

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use common::{CatalogWidget, FakeStore, Listing, Sandbox};
use libplasmoid_updater::{Config, FailureKind, Reporter, update};

struct Quiet;

impl Reporter for Quiet {}

const WIDGET_DIR: &str = "org.fake.widget";
/// The directory name of the icon theme one test installs, in `icons/`.
const ICON_DIR: &str = "FakeIcons";
/// KDE Store category id for icon themes.
const ICON_CATEGORY: u16 = 132;
/// KDE Store category id for plasma styles.
const STYLE_CATEGORY: u16 = 104;
/// KDE Store category id for one of the wallpaper size categories.
const WALLPAPER_CATEGORY: u16 = 298;
/// A file the installed widget holds beside its metadata, which the store's
/// package does not carry.
const KEPT_FILE: &str = "kept-by-the-user";
const KEPT_CONTENT: &str = "added before the run\n";
/// gzip's magic followed by bytes no gzip reader accepts.
const CORRUPT_GZIP: &[u8] = &[0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad, 0xbe, 0xef];

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

fn store_widget() -> CatalogWidget {
    CatalogWidget {
        id: 424_242,
        name: "Fake Widget",
        type_id: 713,
        version: "2.0",
    }
}

fn widget_dir(sandbox: &Sandbox) -> PathBuf {
    sandbox
        .data_home()
        .join("plasma/plasmoids")
        .join(WIDGET_DIR)
}

/// Installs the widget at 1.0 with [`KEPT_FILE`] in it, and returns its
/// directory's inode.
fn install_widget(sandbox: &Sandbox) -> u64 {
    let dir = widget_dir(sandbox);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        format!(
            r#"{{"KPlugin": {{"Name": "Fake Widget", "Id": "{WIDGET_DIR}", "Version": "1.0"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join(KEPT_FILE), KEPT_CONTENT).unwrap();
    std::fs::metadata(&dir).unwrap().ino()
}

/// Installs the widget at 1.0 with no plugin id in its metadata, so the
/// package's own plugin id is not what a test's run fails on, and returns its
/// directory's inode.
fn install_widget_without_a_plugin_id(sandbox: &Sandbox) -> u64 {
    let dir = widget_dir(sandbox);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Fake Widget", "Version": "1.0"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join(KEPT_FILE), KEPT_CONTENT).unwrap();
    std::fs::metadata(&dir).unwrap().ino()
}

/// A `kpackagetool6` that fails every call, so a test reaching the install
/// step never runs the host's real tool.
fn refuse_kpackagetool(sandbox: &Sandbox) {
    sandbox.fake_tool(
        "kpackagetool6",
        "echo \"kpackagetool6 ran: $*\" >&2\nexit 1\n",
    );
}

/// A `kpackagetool6` answering a package it cannot install the way the real
/// tool answers one: exit 2 with its not-installed message.
fn kpackagetool_answering_not_installed(sandbox: &Sandbox) {
    sandbox.fake_tool(
        "kpackagetool6",
        "echo 'Error: Plugin metadata is not installed.' >&2\nexit 2\n",
    );
}

/// Every path under the run's backups dir named `name`.
fn backups_named(sandbox: &Sandbox, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![sandbox.cache_dir().join("plasmoid-updater/backups")];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
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

fn assert_untouched(sandbox: &Sandbox, inode: u64) {
    let dir = widget_dir(sandbox);
    assert_eq!(
        std::fs::metadata(&dir).unwrap().ino(),
        inode,
        "the widget's directory was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(KEPT_FILE)).unwrap(),
        KEPT_CONTENT,
        "the file added before the run is gone"
    );
    let backups = backups_named(sandbox, WIDGET_DIR);
    assert!(backups.is_empty(), "the widget was backed up: {backups:?}");
}

/// The child's run: one update that fails the widget as `kind`, its message
/// holding `message` when one is given.
fn assert_one_failure(child: common::Child, kind: FailureKind, message: Option<&str>) {
    let result = update(&config(&child.store_url), &Quiet).unwrap();
    assert!(result.succeeded.is_empty(), "{result:?}");
    assert_eq!(result.failed.len(), 1, "{result:?}");
    assert_eq!(result.failed[0].kind, kind, "{result:?}");
    if let Some(message) = message {
        assert!(result.failed[0].message.contains(message), "{result:?}");
    }
    child.done();
}

/// The host's `bsdtar`, which the sandbox's own `bsdtar` wraps.
fn real_bsdtar() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("bsdtar"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("no bsdtar on PATH"))
}

/// A `bsdtar` that writes `added` while the archive is listed, the first step
/// of extraction, then runs the real tool: a file the user writes while the
/// run extracts.
fn bsdtar_adding(sandbox: &Sandbox, added: &Path) {
    sandbox.fake_tool(
        "bsdtar",
        &format!(
            "if [ \"$1\" = \"-t\" ]; then printf 'written during extraction\\n' > '{}'; fi\nexec '{}' \"$@\"\n",
            added.display(),
            real_bsdtar().display()
        ),
    );
}

#[test]
fn an_archive_that_fails_extraction_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(child, FailureKind::Extract, None);
        return;
    }
    let sandbox = Sandbox::new();
    let inode = install_widget(&sandbox);
    let added = widget_dir(&sandbox).join("added-during-extraction");
    bsdtar_adding(&sandbox, &added);
    refuse_kpackagetool(&sandbox);
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_download: Some(CORRUPT_GZIP),
        ..Listing::default()
    });
    sandbox.run_child(
        "an_archive_that_fails_extraction_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_untouched(&sandbox, inode);
    assert_eq!(
        std::fs::read_to_string(&added).unwrap(),
        "written during extraction\n",
        "the file written while the run extracted is gone"
    );
}

#[test]
fn a_package_of_another_widget_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("store entry is a different widget"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let inode = install_widget(&sandbox);
    refuse_kpackagetool(&sandbox);
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some("org.someone.else"),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_package_of_another_widget_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_untouched(&sandbox, inode);
}

#[test]
fn a_package_with_a_link_leaving_it_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("outside the package root"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let inode = install_widget(&sandbox);
    refuse_kpackagetool(&sandbox);
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        widget_package_link: true,
        ..Listing::default()
    });
    sandbox.run_child(
        "a_package_with_a_link_leaving_it_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_untouched(&sandbox, inode);
}

#[test]
fn a_download_failing_its_md5_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(child, FailureKind::Checksum, Some("checksum mismatch"));
        return;
    }
    let sandbox = Sandbox::new();
    let inode = install_widget(&sandbox);
    refuse_kpackagetool(&sandbox);
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        widget_md5: Some("00000000000000000000000000000000"),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_download_failing_its_md5_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_untouched(&sandbox, inode);
}

#[test]
fn an_install_that_writes_the_widget_then_fails_restores_its_backup() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("Could not install package"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    install_widget(&sandbox);
    let dir = widget_dir(&sandbox);
    sandbox.fake_tool(
        "kpackagetool6",
        &format!(
            "printf 'half\\n' > '{}'\necho 'Error: Could not install package' >&2\nexit 7\n",
            dir.join("partial").display()
        ),
    );
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_package_id: Some(WIDGET_DIR),
        ..Listing::default()
    });
    sandbox.run_child(
        "an_install_that_writes_the_widget_then_fails_restores_its_backup",
        store.url(),
    );
    assert!(
        !dir.join("partial").exists(),
        "the failed install's write was not rolled back"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(KEPT_FILE)).unwrap(),
        KEPT_CONTENT,
        "the restored widget lost the file its backup held"
    );
    assert_eq!(
        backups_named(&sandbox, WIDGET_DIR).len(),
        1,
        "no backup was taken"
    );
}

/// A widget package with no metadata at all: the package-root lookup fails
/// before any write.
#[test]
fn a_widget_package_without_metadata_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("no metadata.json or metadata.desktop found in package"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let inode = install_widget_without_a_plugin_id(&sandbox);
    refuse_kpackagetool(&sandbox);
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_files: Some(&[("README", "no metadata here\n")]),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_widget_package_without_metadata_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_untouched(&sandbox, inode);
}

/// A PlasmaStyle (a type with a direct fallback) whose package names no
/// plugin id and whose archive holds no theme structure: `kpackagetool6`
/// never installs an id-less package, so neither route can install it, and
/// the refusal comes before any write.
#[test]
fn an_id_less_package_for_a_fallback_type_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("no valid Plasma Style structure found in archive"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let style = sandbox.data_home().join("plasma/desktoptheme/IdLessStyle");
    std::fs::create_dir_all(&style).unwrap();
    std::fs::write(
        style.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Id Less Style", "Version": "1.0"}}"#,
    )
    .unwrap();
    std::fs::write(style.join(KEPT_FILE), KEPT_CONTENT).unwrap();
    let inode = std::fs::metadata(&style).unwrap().ino();
    common::write_registry(
        &sandbox,
        "plasma-themes.knsregistry",
        STYLE_CATEGORY,
        "Id Less Style",
        "1.0",
        &format!("{}/metadata.json", style.display()),
        Some(424_247),
    );
    kpackagetool_answering_not_installed(&sandbox);
    let store = FakeStore::start(Listing {
        extras: vec![common::ExtraEntry {
            id: 424_247,
            name: "Id Less Style",
            type_id: STYLE_CATEGORY,
            version: "2.0",
            files: &[
                (
                    "metadata.json",
                    r#"{"KPackageStructure":"Plasma/Theme","KPlugin":{"Name":"Id Less Style","Version":"2.0"}}"#,
                ),
                ("contents/colors", "new-colors"),
            ],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "an_id_less_package_for_a_fallback_type_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_eq!(
        std::fs::metadata(&style).unwrap().ino(),
        inode,
        "the style's directory was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(style.join(KEPT_FILE)).unwrap(),
        KEPT_CONTENT,
        "the file added before the run is gone"
    );
    let backups = backups_named(&sandbox, "IdLessStyle");
    assert!(backups.is_empty(), "the style was backed up: {backups:?}");
}

/// A PlasmaStyle whose installed side and package name no plugin id and whose
/// archive holds the theme's `colors/`: the package route is one
/// `kpackagetool6` never installs, so the direct install runs and the tool
/// never does.
#[test]
fn an_id_less_fallback_package_installs_directly_without_the_tool() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert!(result.failed.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let style = sandbox.data_home().join("plasma/desktoptheme/IdLessDirect");
    std::fs::create_dir_all(&style).unwrap();
    std::fs::write(
        style.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Id Less Direct", "Version": "1.0"}}"#,
    )
    .unwrap();
    common::write_registry(
        &sandbox,
        "plasma-themes.knsregistry",
        STYLE_CATEGORY,
        "Id Less Direct",
        "1.0",
        &format!("{}/metadata.json", style.display()),
        Some(424_248),
    );
    let tool_marker = sandbox.tmp_dir().join("kpackagetool-ran");
    sandbox.fake_tool(
        "kpackagetool6",
        &format!("printf 'ran\\n' > '{}'\nexit 2\n", tool_marker.display()),
    );
    let store = FakeStore::start(Listing {
        extras: vec![common::ExtraEntry {
            id: 424_248,
            name: "Id Less Direct",
            type_id: STYLE_CATEGORY,
            version: "2.0",
            files: &[
                (
                    "metadata.json",
                    r#"{"KPlugin": {"Name": "Id Less Direct", "Version": "2.0"}}"#,
                ),
                ("colors/colors", "new-colors"),
            ],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "an_id_less_fallback_package_installs_directly_without_the_tool",
        store.url(),
    );
    assert_eq!(
        std::fs::read_to_string(style.join("colors/colors")).unwrap(),
        "new-colors",
        "the direct install did not run"
    );
    assert!(
        !tool_marker.exists(),
        "kpackagetool6 ran for a package it never installs"
    );
}

/// An over-cap `metadata.desktop` names the id only `kpackagetool6` could
/// read; with no readable id against an installed component that has one, the
/// package is refused before the slot: no backup is taken, the widget is
/// untouched, and the tool never runs.
#[test]
fn an_over_cap_desktop_package_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("is larger than the 1048576-byte metadata cap"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let inode = install_widget(&sandbox);
    let tool_marker = sandbox.tmp_dir().join("kpackagetool-ran");
    sandbox.fake_tool(
        "kpackagetool6",
        &format!("printf 'ran\\n' > '{}'\nexit 2\n", tool_marker.display()),
    );
    let desktop = format!(
        "[Desktop Entry]\nX-KDE-PluginInfo-Name={WIDGET_DIR}\n#{}",
        "p".repeat(1024 * 1024)
    );
    let archive = common::files_archive(&[("org.fake.widget/metadata.desktop", &desktop)]);
    let download: &'static [u8] = Box::leak(archive.into_boxed_slice());
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_download: Some(download),
        ..Listing::default()
    });
    sandbox.run_child(
        "an_over_cap_desktop_package_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_untouched(&sandbox, inode);
    assert!(
        !tool_marker.exists(),
        "kpackagetool6 ran for a package it cannot address"
    );
}

/// A PlasmaStyle (a type with a direct fallback) whose package holds no
/// package root but whose archive holds a `colors/` directory: the package
/// route refuses and the direct fallback installs the theme.
#[test]
fn a_plasma_style_with_only_colors_installs_through_the_direct_fallback() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Unverified, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let style = sandbox
        .data_home()
        .join("plasma/desktoptheme/ColorsOnlyStyle");
    std::fs::create_dir_all(&style).unwrap();
    std::fs::write(
        style.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Colors Only Style", "Version": "1.0"}}"#,
    )
    .unwrap();
    common::write_registry(
        &sandbox,
        "plasma-themes.knsregistry",
        STYLE_CATEGORY,
        "Colors Only Style",
        "1.0",
        &format!("{}/metadata.json", style.display()),
        Some(424_245),
    );
    let tool_marker = sandbox.tmp_dir().join("kpackagetool-ran");
    sandbox.fake_tool(
        "kpackagetool6",
        &format!("printf 'ran\\n' > '{}'\nexit 0\n", tool_marker.display()),
    );
    let store = FakeStore::start(Listing {
        extras: vec![common::ExtraEntry {
            id: 424_245,
            name: "Colors Only Style",
            type_id: STYLE_CATEGORY,
            version: "2.0",
            files: &[("colors/colors", "new-colors")],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "a_plasma_style_with_only_colors_installs_through_the_direct_fallback",
        store.url(),
    );
    assert_eq!(
        std::fs::read_to_string(style.join("colors/colors")).unwrap(),
        "new-colors",
        "the direct fallback did not install the theme"
    );
    assert!(
        !tool_marker.exists(),
        "kpackagetool6 ran for a package with no package root"
    );
}

/// A wallpaper whose store archive holds no image: the lookup fails before
/// any write.
#[test]
fn a_wallpaper_archive_without_an_image_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("no wallpaper found in archive"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let wallpaper = sandbox.data_home().join("wallpapers/Nature.jpg");
    std::fs::create_dir_all(wallpaper.parent().unwrap()).unwrap();
    std::fs::write(&wallpaper, "old-wallpaper").unwrap();
    let inode = std::fs::metadata(&wallpaper).unwrap().ino();
    common::write_registry(
        &sandbox,
        "wallpaper.knsregistry",
        WALLPAPER_CATEGORY,
        "Nature",
        "1.0",
        &wallpaper.display().to_string(),
        Some(424_246),
    );
    let store = FakeStore::start(Listing {
        extras: vec![common::ExtraEntry {
            id: 424_246,
            name: "Nature",
            type_id: WALLPAPER_CATEGORY,
            version: "2.0",
            files: &[("README", "no image here\n")],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "a_wallpaper_archive_without_an_image_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_eq!(
        std::fs::metadata(&wallpaper).unwrap().ino(),
        inode,
        "the wallpaper file was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(&wallpaper).unwrap(),
        "old-wallpaper",
        "the installed wallpaper changed"
    );
    let backups = backups_named(&sandbox, "Nature.jpg");
    assert!(
        backups.is_empty(),
        "the wallpaper was backed up: {backups:?}"
    );
}

/// A PlasmaStyle (a type with a direct fallback) whose package holds no
/// package root and whose archive holds no theme structure: neither route can
/// install it, so the refusal comes before any write.
#[test]
fn a_plasma_style_with_no_installable_route_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("no valid Plasma Style structure found in archive"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let style = sandbox.data_home().join("plasma/desktoptheme/NoRouteStyle");
    std::fs::create_dir_all(&style).unwrap();
    std::fs::write(
        style.join("metadata.json"),
        r#"{"KPlugin": {"Name": "No Route Style", "Version": "1.0"}}"#,
    )
    .unwrap();
    std::fs::write(style.join(KEPT_FILE), KEPT_CONTENT).unwrap();
    let inode = std::fs::metadata(&style).unwrap().ino();
    common::write_registry(
        &sandbox,
        "plasma-themes.knsregistry",
        STYLE_CATEGORY,
        "No Route Style",
        "1.0",
        &format!("{}/metadata.json", style.display()),
        Some(424_244),
    );
    refuse_kpackagetool(&sandbox);
    let store = FakeStore::start(Listing {
        extras: vec![common::ExtraEntry {
            id: 424_244,
            name: "No Route Style",
            type_id: STYLE_CATEGORY,
            version: "2.0",
            files: &[("README", "no theme structure here\n")],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "a_plasma_style_with_no_installable_route_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_eq!(
        std::fs::metadata(&style).unwrap().ino(),
        inode,
        "the style's directory was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(style.join(KEPT_FILE)).unwrap(),
        KEPT_CONTENT,
        "the file added before the run is gone"
    );
    let backups = backups_named(&sandbox, "NoRouteStyle");
    assert!(backups.is_empty(), "the style was backed up: {backups:?}");
}

/// A widget package whose metadata records no plugin id: `kpackagetool6`
/// cannot install it, so the refusal comes before any write.
#[test]
fn an_id_less_package_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(child, FailureKind::Install, Some("records no plugin id"));
        return;
    }
    let sandbox = Sandbox::new();
    let inode = install_widget_without_a_plugin_id(&sandbox);
    kpackagetool_answering_not_installed(&sandbox);
    let store = FakeStore::start(Listing {
        widget: Some(store_widget()),
        widget_files: Some(&[(
            "org.fake.widget/metadata.json",
            r#"{"KPlugin": {"Name": "Fake Widget", "Version": "2.0"}}"#,
        )]),
        ..Listing::default()
    });
    sandbox.run_child(
        "an_id_less_package_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_untouched(&sandbox, inode);
}

/// A color scheme (the component is one file) whose store archive carries no
/// scheme file: the install-path lookup fails before any write.
#[test]
fn a_color_scheme_archive_without_a_scheme_file_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("no color scheme file found in archive"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let scheme = sandbox.scheme_path();
    let inode = std::fs::metadata(&scheme).unwrap().ino();
    let store = FakeStore::start(Listing {
        scheme_files: Some(&[("README", "no color scheme here\n")]),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_color_scheme_archive_without_a_scheme_file_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_eq!(
        std::fs::metadata(&scheme).unwrap().ino(),
        inode,
        "the color scheme file was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(&scheme).unwrap(),
        common::OLD_CONTENT,
        "the installed color scheme changed"
    );
    let backups = backups_named(&sandbox, common::SCHEME_FILE);
    assert!(
        backups.is_empty(),
        "the color scheme was backed up: {backups:?}"
    );
}

/// A color scheme whose store serves a zero-byte download: the component
/// fails naming what the store served, before any write.
#[test]
fn an_empty_scheme_download_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("the store served an empty file"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let scheme = sandbox.scheme_path();
    let inode = std::fs::metadata(&scheme).unwrap().ino();
    let store = FakeStore::start(Listing {
        scheme_download: Some(b""),
        ..Listing::default()
    });
    sandbox.run_child(
        "an_empty_scheme_download_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_eq!(
        std::fs::metadata(&scheme).unwrap().ino(),
        inode,
        "the color scheme file was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(&scheme).unwrap(),
        common::OLD_CONTENT,
        "the installed color scheme changed"
    );
    let backups = backups_named(&sandbox, common::SCHEME_FILE);
    assert!(
        backups.is_empty(),
        "the color scheme was backed up: {backups:?}"
    );
}

/// An icon theme (a direct install of one directory) whose store archive
/// carries no `index.theme`: the install-path lookup fails before any write.
#[test]
fn an_icon_theme_archive_without_an_index_theme_takes_no_backup_and_restores_nothing() {
    if let Some(child) = common::child() {
        assert_one_failure(
            child,
            FailureKind::Install,
            Some("no theme (index.theme) found in archive"),
        );
        return;
    }
    let sandbox = Sandbox::new();
    let icons = sandbox.data_home().join("icons").join(ICON_DIR);
    std::fs::create_dir_all(&icons).unwrap();
    std::fs::write(icons.join("index.theme"), "[Icon Theme]\nName=Fake Icons\n").unwrap();
    std::fs::write(icons.join(KEPT_FILE), KEPT_CONTENT).unwrap();
    let inode = std::fs::metadata(&icons).unwrap().ino();
    common::write_registry(
        &sandbox,
        "icons.knsregistry",
        ICON_CATEGORY,
        "Fake Icons",
        "1.0",
        &icons.display().to_string(),
        Some(424_243),
    );
    let store = FakeStore::start(Listing {
        extras: vec![common::ExtraEntry {
            id: 424_243,
            name: "Fake Icons",
            type_id: ICON_CATEGORY,
            version: "2.0",
            files: &[("README", "no theme here\n")],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "an_icon_theme_archive_without_an_index_theme_takes_no_backup_and_restores_nothing",
        store.url(),
    );
    assert_eq!(
        std::fs::metadata(&icons).unwrap().ino(),
        inode,
        "the icon theme's directory was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(icons.join(KEPT_FILE)).unwrap(),
        KEPT_CONTENT,
        "the file added before the run is gone"
    );
    let backups = backups_named(&sandbox, ICON_DIR);
    assert!(
        backups.is_empty(),
        "the icon theme was backed up: {backups:?}"
    );
}
