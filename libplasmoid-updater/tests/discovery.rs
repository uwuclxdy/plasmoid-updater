//! Discovery keeps going when one KNewStuff registry file cannot be parsed
//! (see `common` for the sandboxed child-process harness).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::Sandbox;
use libplasmoid_updater::{ComponentType, Config, get_installed};

const TRUNCATED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Broken Scheme</name>
"#;

fn config() -> Config {
    Config::new()
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

fn scheme_widget(sandbox: &Sandbox, dir: &str, name: &str) {
    let package = sandbox.data_home().join("plasma/plasmoids").join(dir);
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("metadata.json"),
        format!(r#"{{"KPlugin":{{"Name":"{name}","Version":"1.0"}}}}"#),
    )
    .unwrap();
}

fn theme(sandbox: &Sandbox, dir: &str, name: &str) {
    let package = sandbox.data_home().join("plasma/look-and-feel").join(dir);
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("metadata.json"),
        format!(r#"{{"KPlugin":{{"Name":"{name}","Version":"1.0"}}}}"#),
    )
    .unwrap();
}

fn write_registry(sandbox: &Sandbox, file: &str) {
    let dir = sandbox.data_home().join("knewstuff3");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), TRUNCATED).unwrap();
}

#[test]
fn a_broken_registry_only_registry_does_not_hide_other_types() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config()).unwrap();
        let names: Vec<&str> = installed.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Surviving Widget"), "{installed:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    scheme_widget(&sandbox, "surviving-widget", "Surviving Widget");
    write_registry(&sandbox, "colorschemes.knsregistry");
    sandbox.run_child(
        "a_broken_registry_only_registry_does_not_hide_other_types",
        &common::dead_store_url(),
    );
}

#[test]
fn a_broken_shared_path_registry_does_not_hide_other_types() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config()).unwrap();
        let names: Vec<&str> = installed.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Surviving Widget"), "{installed:?}");
        assert!(names.contains(&"Surviving Theme"), "{installed:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    scheme_widget(&sandbox, "surviving-widget", "Surviving Widget");
    theme(&sandbox, "surviving-theme", "Surviving Theme");
    write_registry(&sandbox, "lookandfeel.knsregistry");
    sandbox.run_child(
        "a_broken_shared_path_registry_does_not_hide_other_types",
        &common::dead_store_url(),
    );
}

/// A cursor theme installed the way Discover records it: a metadata-less
/// `~/.icons/<dir>` named by an `xcursor.knsregistry` entry (shape copied from
/// a real registry: category 107, `installedfile` ending in `/*`).
fn cursor_theme(sandbox: &Sandbox) -> std::path::PathBuf {
    let home = sandbox.data_home().parent().unwrap().join("home");
    let theme = home.join(".icons/Test-cursors");
    std::fs::create_dir_all(theme.join("cursors")).unwrap();
    std::fs::write(theme.join("cursors/left_ptr"), b"cursor").unwrap();
    std::fs::write(
        theme.join("index.theme"),
        "[Icon Theme]\nName=Test Cursors\n",
    )
    .unwrap();
    let registry = sandbox.data_home().join("knewstuff3");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::write(
        registry.join("xcursor.knsregistry"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
 <stuff category="107">
  <name>Test Cursors</name>
  <providerid>api.kde-look.org</providerid>
  <version>1.0</version>
  <installedfile>{}/*</installedfile>
  <id>999003</id>
  <status>installed</status>
 </stuff>
</hotnewstuffregistry>
"#,
            theme.display()
        ),
    )
    .unwrap();
    theme
}

#[test]
fn a_registered_cursor_theme_is_listed_as_a_cursor_theme() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config()).unwrap();
        let cursor = installed
            .iter()
            .find(|c| c.name == "Test Cursors")
            .unwrap_or_else(|| panic!("cursor theme not listed: {installed:?}"));
        assert_eq!(cursor.component_type, ComponentType::CursorTheme);
        assert_eq!(cursor.version, "1.0");
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(cursor.path, home.join(".icons/Test-cursors"));
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    cursor_theme(&sandbox);
    sandbox.run_child(
        "a_registered_cursor_theme_is_listed_as_a_cursor_theme",
        &common::dead_store_url(),
    );
}

#[test]
fn a_comic_plugin_with_metadata_is_listed_as_a_comic_plugin() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config()).unwrap();
        let comic = installed
            .iter()
            .find(|c| c.name == "Test Comic")
            .unwrap_or_else(|| panic!("comic plugin not listed: {installed:?}"));
        assert_eq!(comic.component_type, ComponentType::ComicPlugin);
        assert_eq!(comic.version, "1.0");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let package = sandbox
        .data_home()
        .join("plasma/comics/org.example.testcomic");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("metadata.json"),
        r#"{"KPackageStructure":"Plasma/Comic","KPlugin":{"Id":"org.example.testcomic","Name":"Test Comic","Version":"1.0"}}"#,
    )
    .unwrap();
    sandbox.run_child(
        "a_comic_plugin_with_metadata_is_listed_as_a_comic_plugin",
        &common::dead_store_url(),
    );
}

#[test]
fn the_new_types_serialize_in_snake_case() {
    assert_eq!(
        serde_json::to_value(ComponentType::CursorTheme).unwrap(),
        serde_json::Value::String("cursor_theme".to_string())
    );
    assert_eq!(
        serde_json::to_value(ComponentType::ComicPlugin).unwrap(),
        serde_json::Value::String("comic_plugin".to_string())
    );
}

const BREEZE_DARK: &str = "org.kde.breezedark.desktop";

fn system_theme_dir() -> std::path::PathBuf {
    std::path::Path::new("/usr/share/plasma/look-and-feel").join(BREEZE_DARK)
}

#[test]
fn a_user_registry_does_not_choose_a_system_themes_type() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config().with_system(true)).unwrap();
        let theme = installed
            .iter()
            .find(|c| c.directory_name == BREEZE_DARK)
            .unwrap_or_else(|| panic!("the system theme was not discovered: {installed:?}"));
        assert_eq!(
            theme.component_type,
            ComponentType::GlobalTheme,
            "a user-writable registry chose the type of a system component"
        );
        child.done();
        return;
    }

    // The real system theme is the fixture: discovery only reads it.
    if !system_theme_dir().is_dir() {
        return;
    }
    // A trusted system registry may legitimately name the theme, and then this
    // box cannot show the difference.
    if ["lookandfeel.knsregistry", "ksplash.knsregistry"]
        .iter()
        .any(|file| {
            std::path::Path::new("/usr/share/knewstuff3")
                .join(file)
                .exists()
        })
    {
        return;
    }

    let sandbox = Sandbox::new();
    let registry_dir = sandbox.data_home().join("knewstuff3");
    std::fs::create_dir_all(&registry_dir).unwrap();
    std::fs::write(
        registry_dir.join("ksplash.knsregistry"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="708">
    <name>Breeze Dark</name>
    <version>6.0</version>
    <installedfile>{}</installedfile>
    <id>1</id>
  </stuff>
</hotnewstuffregistry>
"#,
            system_theme_dir().display()
        ),
    )
    .unwrap();
    sandbox.run_child(
        "a_user_registry_does_not_choose_a_system_themes_type",
        &common::dead_store_url(),
    );
}
