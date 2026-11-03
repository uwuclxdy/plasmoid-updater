//! Drives the library's public entry points against a local fake KDE Store,
//! each test in its own sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Mutex;

use common::{
    ExtraEntry, FakeStore, Listing, NEW_CONTENT, NEW_VERSION, OLD_CONTENT, OLD_VERSION,
    SCHEME_NAME, Sandbox, assert_content,
};
use libplasmoid_updater::{
    ComponentType, Config, Error, Event, FailureKind, Reporter, check, get_installed, install,
    update,
};

struct Quiet;

impl Reporter for Quiet {}

#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl Reporter for Recording {
    fn event(&self, event: &Event) {
        self.0.lock().unwrap().push(format!("{event:?}"));
    }
}

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

fn version_line(version: &str) -> String {
    format!("<version>{version}</version>")
}

#[test]
fn failure_kinds_serialize_in_snake_case_like_component_types() {
    assert_eq!(
        serde_json::to_string(&FailureKind::NoMatchingDownload).unwrap(),
        "\"no_matching_download\""
    );
    assert_eq!(
        serde_json::to_string(&ComponentType::ColorScheme).unwrap(),
        "\"color_scheme\""
    );
}

#[cfg(feature = "cli")]
#[test]
fn select_of_nothing_is_empty_without_a_terminal() {
    if let Some(child) = common::child() {
        let picked = libplasmoid_updater::cli::select(&[]).unwrap();
        assert!(picked.is_empty());
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.run_child(
        "select_of_nothing_is_empty_without_a_terminal",
        &common::dead_store_url(),
    );
}

#[test]
fn get_installed_lists_the_registered_color_scheme() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config(&child.store_url)).unwrap();
        assert_eq!(installed.len(), 1, "{installed:?}");
        assert_eq!(installed[0].name, SCHEME_NAME);
        assert_eq!(installed[0].version, OLD_VERSION);
        assert_eq!(installed[0].component_type, ComponentType::ColorScheme);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    sandbox.run_child(
        "get_installed_lists_the_registered_color_scheme",
        &common::dead_store_url(),
    );
}

#[test]
fn check_reports_the_newer_store_version() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(available.installed.name, SCHEME_NAME);
        assert_eq!(available.latest_version, NEW_VERSION);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("check_reports_the_newer_store_version", store.url());
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn check_reports_nothing_when_the_store_has_the_installed_version() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        version: OLD_VERSION,
        ..Listing::default()
    });
    sandbox.run_child(
        "check_reports_nothing_when_the_store_has_the_installed_version",
        store.url(),
    );
}

#[test]
fn check_leaves_an_excluded_component_out_and_reports_it_skipped() {
    if let Some(child) = common::child() {
        let cfg = config(&child.store_url).with_excluded_patterns(vec!["(?i)^fake".to_string()]);
        let result = check(&cfg).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert_eq!(result.skipped, vec![SCHEME_NAME.to_string()]);
        assert!(
            !result.is_empty(),
            "a check that skipped a component is not empty"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child(
        "check_leaves_an_excluded_component_out_and_reports_it_skipped",
        store.url(),
    );
}

#[test]
fn check_turns_a_failed_lookup_into_a_diagnostic() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].name, SCHEME_NAME);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        in_catalog: false,
        per_id_fails: true,
        ..Listing::default()
    });
    sandbox.run_child("check_turns_a_failed_lookup_into_a_diagnostic", store.url());
}

#[test]
fn check_turns_a_dropped_lookup_on_a_reachable_store_into_a_diagnostic() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert_eq!(result.diagnostics.len(), 1, "{result:?}");
        assert_eq!(result.diagnostics[0].name, SCHEME_NAME);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        in_catalog: false,
        per_id_drops: true,
        ..Listing::default()
    });
    sandbox.run_child(
        "check_turns_a_dropped_lookup_on_a_reachable_store_into_a_diagnostic",
        store.url(),
    );
}

#[test]
fn check_retries_a_lookup_the_store_briefly_refuses() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        assert!(result.diagnostics.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        in_catalog: false,
        per_id_fails_first: 2,
        ..Listing::default()
    });
    sandbox.run_child(
        "check_retries_a_lookup_the_store_briefly_refuses",
        store.url(),
    );
}

#[test]
fn check_against_an_unreachable_store_is_a_network_error() {
    if let Some(child) = common::child() {
        let err = check(&config(&child.store_url)).unwrap_err();
        assert!(matches!(err, Error::Network(_)), "{err:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    sandbox.run_child(
        "check_against_an_unreachable_store_is_a_network_error",
        &common::dead_store_url(),
    );
}

#[test]
fn update_installs_the_newer_version() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert!(result.failed.is_empty(), "{result:?}");
        assert!(
            !result.needs_restart(),
            "a color scheme needs no plasmashell restart"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("update_installs_the_newer_version", store.url());
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
    let registry = sandbox.registry_content();
    assert!(
        registry.contains(&version_line(NEW_VERSION)),
        "registry not bumped to {NEW_VERSION}"
    );
    assert!(
        !registry.contains(&version_line(OLD_VERSION)),
        "registry still lists {OLD_VERSION}"
    );
}

#[test]
fn update_reports_progress_through_the_reporter() {
    if let Some(child) = common::child() {
        let reporter = Recording::default();
        update(&config(&child.store_url), &reporter).unwrap();
        let events = reporter.0.into_inner().unwrap();
        assert!(
            events.iter().any(|e| e.starts_with("Downloading")),
            "no Downloading event: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.starts_with("Installed")),
            "no Installed event: {events:?}"
        );
        let display_name = format!("name: {SCHEME_NAME:?}");
        assert!(
            events
                .iter()
                .filter(|e| e.starts_with("Downloading") || e.starts_with("Installed"))
                .all(|e| e.contains(&display_name)),
            "events do not carry the display name: {events:?}"
        );
        let installed_path = std::path::Path::new(&std::env::var("XDG_DATA_HOME").unwrap())
            .join("color-schemes")
            .join(common::SCHEME_FILE);
        let id = format!("id: {:?}", installed_path.display().to_string());
        assert!(
            events
                .iter()
                .filter(|e| e.starts_with("Downloading") || e.starts_with("Installed"))
                .all(|e| e.contains(&id)),
            "events are not keyed by the installed path: {events:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("update_reports_progress_through_the_reporter", store.url());
}

#[test]
fn update_reports_a_checksum_failure_and_keeps_the_old_version() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert!(result.succeeded.is_empty(), "{result:?}");
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].component.name, SCHEME_NAME);
        assert_eq!(result.failed[0].kind, FailureKind::Checksum);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing {
        md5_override: Some("00000000000000000000000000000000"),
        ..Listing::default()
    });
    sandbox.run_child(
        "update_reports_a_checksum_failure_and_keeps_the_old_version",
        store.url(),
    );
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
    assert!(
        sandbox
            .registry_content()
            .contains(&version_line(OLD_VERSION))
    );
}

#[test]
fn install_of_an_empty_selection_changes_nothing() {
    if let Some(child) = common::child() {
        let result = install(&[], &config(&child.store_url), &Quiet).unwrap();
        assert!(
            result.succeeded.is_empty() && result.failed.is_empty(),
            "{result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("install_of_an_empty_selection_changes_nothing", store.url());
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn update_skips_an_excluded_component() {
    if let Some(child) = common::child() {
        let cfg = config(&child.store_url).with_excluded_packages(vec![SCHEME_NAME.to_string()]);
        let result = update(&cfg, &Quiet).unwrap();
        assert!(
            result.succeeded.is_empty() && result.failed.is_empty(),
            "{result:?}"
        );
        assert_eq!(result.skipped, vec![SCHEME_NAME.to_string()]);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("update_skips_an_excluded_component", store.url());
    assert_content(&sandbox.scheme_path(), OLD_CONTENT);
}

#[test]
fn install_installs_exactly_the_given_selection() {
    if let Some(child) = common::child() {
        let available = check(&config(&child.store_url)).unwrap().available_updates;
        let excluding =
            config(&child.store_url).with_excluded_packages(vec![SCHEME_NAME.to_string()]);
        let result = install(&available, &excluding, &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert!(result.skipped.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("install_installs_exactly_the_given_selection", store.url());
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}

#[test]
fn install_applies_the_selection_from_check() {
    if let Some(child) = common::child() {
        let cfg = config(&child.store_url);
        let available = check(&cfg).unwrap().available_updates;
        let result = install(&available, &cfg, &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert!(result.failed.is_empty(), "{result:?}");
        assert_eq!(
            result.succeeded[0].version, NEW_VERSION,
            "a succeeded row carries the installed version: {result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("install_applies_the_selection_from_check", store.url());
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}

#[cfg(feature = "cli")]
#[test]
fn the_table_renderers_take_the_stream_they_are_printed_on() {
    use libplasmoid_updater::cli::{Stream, components_table, updates_table};
    let updates: fn(&[libplasmoid_updater::AvailableUpdate], Stream) -> String = updates_table;
    let components: fn(&[libplasmoid_updater::InstalledComponent], Stream) -> String =
        components_table;
    assert!(updates(&[], Stream::Stderr).contains("NAME"));
    assert!(components(&[], Stream::Stdout).contains("NAME"));
}

#[test]
fn every_component_type_serializes_to_its_spelled_out_snake_case_name() {
    let expected = [
        (ComponentType::PlasmaWidget, "plasma_widget"),
        (ComponentType::WallpaperPlugin, "wallpaper_plugin"),
        (ComponentType::KWinEffect, "kwin_effect"),
        (ComponentType::KWinScript, "kwin_script"),
        (ComponentType::KWinSwitcher, "kwin_switcher"),
        (ComponentType::GlobalTheme, "global_theme"),
        (ComponentType::PlasmaStyle, "plasma_style"),
        (ComponentType::AuroraeDecoration, "aurorae_decoration"),
        (ComponentType::ColorScheme, "color_scheme"),
        (ComponentType::SplashScreen, "splash_screen"),
        (ComponentType::SddmTheme, "sddm_theme"),
        (ComponentType::IconTheme, "icon_theme"),
        (ComponentType::Wallpaper, "wallpaper"),
        (ComponentType::CursorTheme, "cursor_theme"),
        (ComponentType::ComicPlugin, "comic_plugin"),
    ];
    for (component_type, name) in expected {
        assert_eq!(
            serde_json::to_value(component_type).unwrap(),
            serde_json::Value::String(name.to_string()),
            "{component_type:?}"
        );
        let back: ComponentType =
            serde_json::from_value(serde_json::Value::String(name.to_string())).unwrap();
        assert_eq!(back, component_type);
    }
}

// -- Cursor themes and comic plugins --

const CURSOR_ID: u64 = 999_011;
const CURSOR_NAME: &str = "Test Cursors";
const CURSOR_CATEGORY: u16 = 107;
const COMIC_ID: u64 = 999_012;
const COMIC_NAME: &str = "Test Comic";
const COMIC_CATEGORY: u16 = 155;
const COMIC_PACKAGE: &str = "org.example.testcomic";

/// A wallpaper registered from one of `wallpaper.knsrc`'s six categories. The
/// store's "KDE Wallpaper 1440x900" category is 298 (measured 2026-11-03).
const WALLPAPER_ID: u64 = 998_298;
const WALLPAPER_NAME: &str = "Nature";
const WALLPAPER_SIZE_CATEGORY: u16 = 298;

/// A widget whose store name differs from its metadata name, registered with a
/// store id the widget must resolve through its registry, not its name.
const WIDGET_ID: u64 = 999_031;

/// Writes one KNewStuff registry file holding one entry; `id` is omitted when
/// `None`, which leaves the entry out of the id cache.
fn write_registry(
    sandbox: &Sandbox,
    file: &str,
    category: u16,
    name: &str,
    version: &str,
    installed_file: &str,
    id: Option<u64>,
) {
    let registry = sandbox.data_home().join("knewstuff3").join(file);
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    let id_element = id.map(|id| format!("<id>{id}</id>")).unwrap_or_default();
    std::fs::write(
        &registry,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
  <stuff category="{category}">
    <name>{name}</name>
    <providerid>api.kde-look.org</providerid>
    <version>{version}</version>
    <installedfile>{installed_file}</installedfile>
    {id_element}
    <status>installed</status>
  </stuff>
</hotnewstuffregistry>
"#
        ),
    )
    .unwrap();
}

fn cursor_theme_path(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.home_dir().join(".icons/Test-cursors")
}

/// Installs a metadata-less cursor theme where KNewStuff's `xcursor.knsrc`
/// puts it, registered the way the real `xcursor.knsregistry` records it:
/// category 107 and an `installedfile` ending in `/*`.
fn install_cursor_theme(sandbox: &Sandbox, version: &str) {
    let theme = cursor_theme_path(sandbox);
    std::fs::create_dir_all(theme.join("cursors")).unwrap();
    std::fs::write(
        theme.join("index.theme"),
        "[Icon Theme]\nName=Test Cursors\n",
    )
    .unwrap();
    std::fs::write(theme.join("cursors/left_ptr"), "old-cursor").unwrap();
    write_registry(
        sandbox,
        "xcursor.knsregistry",
        CURSOR_CATEGORY,
        CURSOR_NAME,
        version,
        &format!("{}/*", theme.display()),
        Some(CURSOR_ID),
    );
}

fn install_old_cursor_theme(sandbox: &Sandbox) {
    install_cursor_theme(sandbox, "1.0");
}

fn comic_path(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox
        .data_home()
        .join("plasma/comics")
        .join(COMIC_PACKAGE)
}

/// The first file named `name` anywhere under `dir`.
fn find_named(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_named(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|entry| entry == name) {
            return Some(path);
        }
    }
    None
}

fn install_old_comic_plugin(sandbox: &Sandbox) {
    let package = comic_path(sandbox);
    std::fs::create_dir_all(package.join("contents/ui")).unwrap();
    std::fs::write(
        package.join("metadata.json"),
        format!(
            r#"{{"KPackageStructure":"Plasma/Comic","KPlugin":{{"Id":"{COMIC_PACKAGE}","Name":"{COMIC_NAME}","Version":"1.0"}}}}"#
        ),
    )
    .unwrap();
    write_registry(
        sandbox,
        "comic.knsregistry",
        COMIC_CATEGORY,
        COMIC_NAME,
        "1.0",
        &format!("{}/metadata.json", package.display()),
        Some(COMIC_ID),
    );
}

fn wallpaper_path(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.data_home().join("wallpapers/Nature.jpg")
}

/// Installs a single-file wallpaper registered under one of `wallpaper.knsrc`'s
/// six categories (298), the way Discover records a store install.
fn install_old_wallpaper(sandbox: &Sandbox) {
    let wallpaper = wallpaper_path(sandbox);
    std::fs::create_dir_all(wallpaper.parent().unwrap()).unwrap();
    std::fs::write(&wallpaper, "old-wallpaper").unwrap();
    write_registry(
        sandbox,
        "wallpaper.knsregistry",
        WALLPAPER_SIZE_CATEGORY,
        WALLPAPER_NAME,
        "1.0",
        &wallpaper.display().to_string(),
        Some(WALLPAPER_ID),
    );
}

const CURSOR_ARCHIVE: &[(&str, &str)] = &[
    (
        "Test-cursors/index.theme",
        "[Icon Theme]\nName=Test Cursors\n",
    ),
    ("Test-cursors/cursors/left_ptr", "new-cursor"),
];

const COMIC_METADATA: &str = concat!(
    r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.testcomic","#,
    r#""Name":"Test Comic","Version":"2.0"}}"#
);

const COMIC_ARCHIVE: &[(&str, &str)] = &[
    ("org.example.testcomic/metadata.json", COMIC_METADATA),
    (
        "org.example.testcomic/contents/ui/main.qml",
        "import QtQuick\nItem {}\n",
    ),
];

/// A fake `kpackagetool6` that copies the package it is handed into the user's
/// comic directory, the way `-t Plasma/Comic -u` lands it. Any other type is
/// refused the way the real tool refuses a package of the wrong structure.
const COMIC_KPACKAGETOOL: &str = r#"if [ "$1" != "-t" ] || [ "$2" != "Plasma/Comic" ] || [ "$3" != "-u" ]; then
  echo "not a Plasma/Comic package: $*" >&2
  exit 1
fi
mkdir -p "$XDG_DATA_HOME/plasma/comics"
cp -a "$4" "$XDG_DATA_HOME/plasma/comics/"
exit 0
"#;

#[test]
fn check_reports_a_newer_cursor_theme_installed_under_home_icons() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(available.installed.name, CURSOR_NAME);
        assert_eq!(
            available.installed.component_type,
            ComponentType::CursorTheme
        );
        assert_eq!(available.latest_version, NEW_VERSION);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_cursor_theme(&sandbox);
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: CURSOR_ID,
            name: CURSOR_NAME,
            type_id: CURSOR_CATEGORY,
            version: NEW_VERSION,
            files: CURSOR_ARCHIVE,
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "check_reports_a_newer_cursor_theme_installed_under_home_icons",
        store.url(),
    );
    assert_eq!(
        std::fs::read_to_string(cursor_theme_path(&sandbox).join("cursors/left_ptr")).unwrap(),
        "old-cursor",
        "check changed the installed theme"
    );
}

#[test]
fn update_installs_a_newer_cursor_theme_and_bumps_its_registry_entry() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert!(result.failed.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_cursor_theme(&sandbox);
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: CURSOR_ID,
            name: CURSOR_NAME,
            type_id: CURSOR_CATEGORY,
            version: NEW_VERSION,
            files: CURSOR_ARCHIVE,
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "update_installs_a_newer_cursor_theme_and_bumps_its_registry_entry",
        store.url(),
    );
    let theme = cursor_theme_path(&sandbox);
    assert_eq!(
        std::fs::read_to_string(theme.join("cursors/left_ptr")).unwrap(),
        "new-cursor"
    );
    let backup = find_named(&sandbox.cache_dir(), "left_ptr")
        .expect("the replaced cursor theme was not backed up");
    assert_eq!(
        std::fs::read_to_string(backup).unwrap(),
        "old-cursor",
        "the backup does not hold the replaced theme"
    );
    let registry =
        std::fs::read_to_string(sandbox.data_home().join("knewstuff3/xcursor.knsregistry"))
            .unwrap();
    assert!(
        registry.contains(&version_line(NEW_VERSION)),
        "the cursor registry entry was not bumped: {registry}"
    );
    assert!(
        registry.contains(&format!(
            "<installedfile>{}/*</installedfile>",
            theme.display()
        )),
        "the cursor entry lost its `/*` installedfile: {registry}"
    );
}

#[test]
fn an_empty_cursor_registry_version_is_not_an_update_by_itself() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config(&child.store_url)).unwrap();
        assert_eq!(installed.len(), 1, "{installed:?}");
        assert_eq!(installed[0].component_type, ComponentType::CursorTheme);
        assert_eq!(installed[0].version, "");
        let result = check(&config(&child.store_url)).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert!(result.diagnostics.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_cursor_theme(&sandbox, "");
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: CURSOR_ID,
            name: CURSOR_NAME,
            type_id: CURSOR_CATEGORY,
            version: "",
            files: CURSOR_ARCHIVE,
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "an_empty_cursor_registry_version_is_not_an_update_by_itself",
        store.url(),
    );
}

#[test]
fn an_empty_cursor_registry_version_counts_as_an_update_when_the_store_has_a_version() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(
            available.installed.component_type,
            ComponentType::CursorTheme
        );
        assert_eq!(available.latest_version, NEW_VERSION);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_cursor_theme(&sandbox, "");
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: CURSOR_ID,
            name: CURSOR_NAME,
            type_id: CURSOR_CATEGORY,
            version: NEW_VERSION,
            files: CURSOR_ARCHIVE,
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "an_empty_cursor_registry_version_counts_as_an_update_when_the_store_has_a_version",
        store.url(),
    );
}

#[test]
fn check_reports_a_newer_comic_plugin() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(available.installed.name, COMIC_NAME);
        assert_eq!(
            available.installed.component_type,
            ComponentType::ComicPlugin
        );
        assert_eq!(available.latest_version, NEW_VERSION);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic_plugin(&sandbox);
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: COMIC_ID,
            name: COMIC_NAME,
            type_id: COMIC_CATEGORY,
            version: NEW_VERSION,
            files: COMIC_ARCHIVE,
        }],
        ..Listing::default()
    });
    sandbox.run_child("check_reports_a_newer_comic_plugin", store.url());
}

#[test]
fn update_installs_a_newer_comic_plugin() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert!(result.failed.is_empty(), "{result:?}");
        assert_eq!(
            result.succeeded[0].component_type,
            ComponentType::ComicPlugin
        );
        assert_eq!(result.succeeded[0].version, NEW_VERSION);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_comic_plugin(&sandbox);
    sandbox.fake_tool("kpackagetool6", COMIC_KPACKAGETOOL);
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: COMIC_ID,
            name: COMIC_NAME,
            type_id: COMIC_CATEGORY,
            version: NEW_VERSION,
            files: COMIC_ARCHIVE,
        }],
        ..Listing::default()
    });
    sandbox.run_child("update_installs_a_newer_comic_plugin", store.url());
    let metadata: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(comic_path(&sandbox).join("metadata.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["KPlugin"]["Version"], "2.0", "{metadata}");
    let registry =
        std::fs::read_to_string(sandbox.data_home().join("knewstuff3/comic.knsregistry")).unwrap();
    assert!(
        registry.contains(&version_line(NEW_VERSION)),
        "the comic registry entry was not bumped: {registry}"
    );
}

// -- One directory name under several component types --

const ICON_CATEGORY: u16 = 132;
const STYLE_CATEGORY: u16 = 104;
const SHARED_ICON_ID: u64 = 999_021;
const SHARED_CURSOR_ID: u64 = 999_022;
const SHARED_STYLE_ID: u64 = 999_023;
const NEW_ICON_THEME: &str = "[Icon Theme]\nName=Shared Icon Theme\n# new\n";

#[test]
fn an_icon_theme_and_a_cursor_theme_with_one_directory_name_keep_their_own_ids() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 2, "{result:?}");
        let resolved: Vec<(ComponentType, u64)> = result
            .available_updates
            .iter()
            .map(|available| (available.installed.component_type, available.content_id))
            .collect();
        assert!(
            resolved.contains(&(ComponentType::IconTheme, SHARED_ICON_ID)),
            "the icon theme did not resolve to its own id: {resolved:?}"
        );
        assert!(
            resolved.contains(&(ComponentType::CursorTheme, SHARED_CURSOR_ID)),
            "the cursor theme did not resolve to its own id: {resolved:?}"
        );
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 2, "{result:?}");
        assert!(result.failed.is_empty(), "{result:?}");
        child.done();
        return;
    }

    let sandbox = Sandbox::new();
    let icons = sandbox.data_home().join("icons/Shared");
    std::fs::create_dir_all(&icons).unwrap();
    std::fs::write(
        icons.join("index.theme"),
        "[Icon Theme]\nName=Shared Icon Theme\n",
    )
    .unwrap();
    write_registry(
        &sandbox,
        "icons.knsregistry",
        ICON_CATEGORY,
        "Shared Icon Theme",
        "1.0",
        &icons.display().to_string(),
        Some(SHARED_ICON_ID),
    );

    let cursor = sandbox.home_dir().join(".icons/Shared");
    std::fs::create_dir_all(cursor.join("cursors")).unwrap();
    std::fs::write(
        cursor.join("index.theme"),
        "[Icon Theme]\nName=Shared Cursors\n",
    )
    .unwrap();
    std::fs::write(cursor.join("cursors/left_ptr"), "old-cursor").unwrap();
    write_registry(
        &sandbox,
        "xcursor.knsregistry",
        CURSOR_CATEGORY,
        "Shared Cursors",
        "1.0",
        &format!("{}/*", cursor.display()),
        Some(SHARED_CURSOR_ID),
    );

    let store = FakeStore::start(Listing {
        extras: vec![
            ExtraEntry {
                id: SHARED_ICON_ID,
                name: "Shared Icon Theme",
                type_id: ICON_CATEGORY,
                version: NEW_VERSION,
                files: &[("Shared/index.theme", NEW_ICON_THEME)],
            },
            ExtraEntry {
                id: SHARED_CURSOR_ID,
                name: "Shared Cursors",
                type_id: CURSOR_CATEGORY,
                version: NEW_VERSION,
                files: &[
                    ("Shared/index.theme", "[Icon Theme]\nName=Shared Cursors\n"),
                    ("Shared/cursors/left_ptr", "new-cursor"),
                ],
            },
        ],
        ..Listing::default()
    });
    sandbox.run_child(
        "an_icon_theme_and_a_cursor_theme_with_one_directory_name_keep_their_own_ids",
        store.url(),
    );
    assert_eq!(
        std::fs::read_to_string(icons.join("index.theme")).unwrap(),
        NEW_ICON_THEME,
        "the icon theme did not install its own archive"
    );
    assert!(
        !icons.join("cursors").exists(),
        "the cursor archive landed in the icon theme's directory"
    );
    assert_eq!(
        std::fs::read_to_string(cursor.join("cursors/left_ptr")).unwrap(),
        "new-cursor"
    );
}

#[test]
fn a_plasma_style_and_an_icon_theme_with_one_directory_name_keep_their_own_ids() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert!(
            result.diagnostics.is_empty(),
            "a component did not resolve to its own type's entry: {result:?}"
        );
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(
            available.installed.component_type,
            ComponentType::IconTheme,
            "{result:?}"
        );
        assert_eq!(available.content_id, SHARED_ICON_ID);
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert!(
            result.failed.is_empty(),
            "the plasma style was updated from the icon theme's entry: {result:?}"
        );
        child.done();
        return;
    }

    let sandbox = Sandbox::new();
    let style = sandbox.data_home().join("plasma/desktoptheme/WhiteSur");
    std::fs::create_dir_all(style.join("contents")).unwrap();
    std::fs::write(
        style.join("metadata.json"),
        r#"{"KPlugin":{"Name":"WhiteSur","Version":"1.0"}}"#,
    )
    .unwrap();
    std::fs::write(style.join("contents/colors"), "old-style").unwrap();
    write_registry(
        &sandbox,
        "plasma-themes.knsregistry",
        STYLE_CATEGORY,
        "WhiteSur",
        "1.0",
        &format!("{}/metadata.json", style.display()),
        Some(SHARED_STYLE_ID),
    );

    let icons = sandbox.data_home().join("icons/WhiteSur");
    std::fs::create_dir_all(&icons).unwrap();
    std::fs::write(icons.join("index.theme"), "[Icon Theme]\nName=WhiteSur\n").unwrap();
    write_registry(
        &sandbox,
        "icons.knsregistry",
        ICON_CATEGORY,
        "WhiteSur",
        "1.0",
        &icons.display().to_string(),
        Some(SHARED_ICON_ID),
    );

    let store = FakeStore::start(Listing {
        extras: vec![
            ExtraEntry {
                id: SHARED_STYLE_ID,
                name: "WhiteSur",
                type_id: STYLE_CATEGORY,
                version: "1.0",
                files: &[
                    (
                        "WhiteSur/metadata.json",
                        r#"{"KPlugin":{"Name":"WhiteSur","Version":"1.0"}}"#,
                    ),
                    ("WhiteSur/contents/colors", "old-style"),
                ],
            },
            ExtraEntry {
                id: SHARED_ICON_ID,
                name: "WhiteSur",
                type_id: ICON_CATEGORY,
                version: NEW_VERSION,
                files: &[("WhiteSur/index.theme", NEW_ICON_THEME)],
            },
        ],
        ..Listing::default()
    });
    sandbox.run_child(
        "a_plasma_style_and_an_icon_theme_with_one_directory_name_keep_their_own_ids",
        store.url(),
    );
    assert_eq!(
        std::fs::read_to_string(style.join("contents/colors")).unwrap(),
        "old-style",
        "the icon theme's archive replaced the plasma style"
    );
}

// -- Wallpapers from every `wallpaper.knsrc` category --

#[test]
fn a_wallpaper_registered_from_a_size_category_updates() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(
            available.installed.component_type,
            ComponentType::Wallpaper,
            "{result:?}"
        );
        assert_eq!(available.content_id, WALLPAPER_ID);
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        assert!(result.failed.is_empty(), "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_wallpaper(&sandbox);
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: WALLPAPER_ID,
            name: WALLPAPER_NAME,
            type_id: WALLPAPER_SIZE_CATEGORY,
            version: NEW_VERSION,
            files: &[("Nature.jpg", "new-wallpaper")],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "a_wallpaper_registered_from_a_size_category_updates",
        store.url(),
    );
    assert_eq!(
        std::fs::read_to_string(wallpaper_path(&sandbox)).unwrap(),
        "new-wallpaper",
        "the wallpaper was not installed"
    );
    let registry =
        std::fs::read_to_string(sandbox.data_home().join("knewstuff3/wallpaper.knsregistry"))
            .unwrap();
    assert!(
        registry.contains(&version_line(NEW_VERSION)),
        "the wallpaper registry entry was not bumped: {registry}"
    );
}

#[test]
fn a_wallpaper_is_not_updated_from_a_store_entry_of_another_type() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.reason.contains("different component type")),
            "no type-mismatch diagnostic: {result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    install_old_wallpaper(&sandbox);
    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: WALLPAPER_ID,
            name: WALLPAPER_NAME,
            type_id: CURSOR_CATEGORY,
            version: NEW_VERSION,
            files: &[("Nature.jpg", "new-wallpaper")],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "a_wallpaper_is_not_updated_from_a_store_entry_of_another_type",
        store.url(),
    );
}

// -- A `..`-spelled XDG_DATA_HOME still resolves a widget by its registry id --

#[test]
fn a_widget_resolves_its_registry_id_when_xdg_data_home_is_spelled_with_dot_dot() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(
            available.installed.component_type,
            ComponentType::PlasmaWidget,
            "{result:?}"
        );
        assert_eq!(available.content_id, WIDGET_ID);
        child.done();
        return;
    }

    let sandbox = Sandbox::new();
    let data = std::fs::canonicalize(sandbox.data_home()).unwrap();
    let widget = data.join("plasma/plasmoids/org.example.widget");
    std::fs::create_dir_all(&widget).unwrap();
    std::fs::write(
        widget.join("metadata.json"),
        r#"{"KPlugin":{"Id":"org.example.widget","Name":"Metadata Name","Version":"1.0"}}"#,
    )
    .unwrap();
    write_registry(
        &sandbox,
        "plasmoids.knsregistry",
        705,
        "Store Name",
        "1.0",
        &format!("{}/metadata.json", widget.display()),
        Some(WIDGET_ID),
    );

    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: WIDGET_ID,
            name: "Store Name",
            type_id: 705,
            version: NEW_VERSION,
            files: &[
                (
                    "org.example.widget/metadata.json",
                    r#"{"KPackageStructure":"Plasma/Applet","KPlugin":{"Id":"org.example.widget","Name":"Store Name","Version":"2.0"}}"#,
                ),
                (
                    "org.example.widget/contents/ui/main.qml",
                    "import QtQuick\nItem {}\n",
                ),
            ],
        }],
        ..Listing::default()
    });

    // XDG_DATA_HOME spelled with a `..` that resolves to the same directory,
    // while the registry records the resolved spelling.
    let dotted_data = std::path::PathBuf::from(format!("{}/../data", data.display()));
    sandbox.run_child_with_data_home(
        "a_widget_resolves_its_registry_id_when_xdg_data_home_is_spelled_with_dot_dot",
        store.url(),
        &dotted_data,
    );
}

// -- A `..`-spelled registry path still answers for its own component --

#[test]
fn a_widget_resolves_its_registry_id_from_an_installedfile_spelled_with_dot_dot() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        let available = &result.available_updates[0];
        assert_eq!(
            available.installed.component_type,
            ComponentType::PlasmaWidget,
            "{result:?}"
        );
        assert_eq!(available.content_id, WIDGET_ID);
        child.done();
        return;
    }

    let sandbox = Sandbox::new();
    let widget = sandbox
        .data_home()
        .join("plasma/plasmoids/org.example.widget");
    std::fs::create_dir_all(&widget).unwrap();
    std::fs::write(
        widget.join("metadata.json"),
        r#"{"KPlugin":{"Id":"org.example.widget","Name":"Metadata Name","Version":"1.0"}}"#,
    )
    .unwrap();
    // The same directory spelled through a `..` in the registry.
    let dotted_widget = format!(
        "{}/plasma/plasmoids/../plasmoids/org.example.widget",
        sandbox.data_home().display()
    );
    write_registry(
        &sandbox,
        "plasmoids.knsregistry",
        705,
        "Store Name",
        "1.0",
        &format!("{dotted_widget}/metadata.json"),
        Some(WIDGET_ID),
    );

    let store = FakeStore::start(Listing {
        extras: vec![ExtraEntry {
            id: WIDGET_ID,
            name: "Store Name",
            type_id: 705,
            version: NEW_VERSION,
            files: &[
                (
                    "org.example.widget/metadata.json",
                    r#"{"KPackageStructure":"Plasma/Applet","KPlugin":{"Id":"org.example.widget","Name":"Store Name","Version":"2.0"}}"#,
                ),
                (
                    "org.example.widget/contents/ui/main.qml",
                    "import QtQuick\nItem {}\n",
                ),
            ],
        }],
        ..Listing::default()
    });
    sandbox.run_child(
        "a_widget_resolves_its_registry_id_from_an_installedfile_spelled_with_dot_dot",
        store.url(),
    );
}

// -- A `..`-spelled XDG_DATA_HOME still lists a registry-only component --

#[test]
fn a_registered_icon_theme_is_listed_when_xdg_data_home_is_spelled_with_dot_dot() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config(&child.store_url)).unwrap();
        assert_eq!(installed.len(), 1, "{installed:?}");
        assert_eq!(installed[0].component_type, ComponentType::IconTheme);
        assert_eq!(installed[0].directory_name, "Dotted");
        child.done();
        return;
    }

    let sandbox = Sandbox::new();
    let data = std::fs::canonicalize(sandbox.data_home()).unwrap();
    let icon = data.join("icons/Dotted");
    std::fs::create_dir_all(&icon).unwrap();
    std::fs::write(icon.join("index.theme"), "[Icon Theme]\nName=Dotted\n").unwrap();
    write_registry(
        &sandbox,
        "icons.knsregistry",
        ICON_CATEGORY,
        "Dotted",
        "1.0",
        &icon.display().to_string(),
        Some(SHARED_ICON_ID),
    );

    let dotted_data = std::path::PathBuf::from(format!("{}/../data", data.display()));
    sandbox.run_child_with_data_home(
        "a_registered_icon_theme_is_listed_when_xdg_data_home_is_spelled_with_dot_dot",
        &common::dead_store_url(),
        &dotted_data,
    );
}
