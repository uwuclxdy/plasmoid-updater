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
