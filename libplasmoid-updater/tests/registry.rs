//! Reading the KNewStuff registry through `get_installed`, each test in its own
//! sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::Sandbox;
use libplasmoid_updater::{Config, get_installed};

fn config() -> Config {
    Config::new()
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

/// Installs one color scheme at `file_name`, registered under `xml_name` with
/// its `installedfile` written as `xml_path` (both already XML-escaped).
fn install_scheme(sandbox: &Sandbox, file_name: &str, xml_name: &str, xml_path: &str) {
    let scheme = sandbox.data_home().join("color-schemes").join(file_name);
    std::fs::create_dir_all(scheme.parent().unwrap()).unwrap();
    std::fs::write(&scheme, "[General]\n").unwrap();
    let registry = sandbox.registry_path();
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    std::fs::write(
        &registry,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
  <stuff category="112">
    <name>{xml_name}</name>
    <providerid>api.kde-look.org</providerid>
    <version>1.0</version>
    <installedfile>{xml_path}</installedfile>
    <id>999001</id>
    <status>installed</status>
  </stuff>
</hotnewstuffregistry>
"#
        ),
    )
    .unwrap();
}

#[test]
fn a_registry_name_with_entities_reads_as_the_decoded_name() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config()).unwrap();
        assert_eq!(installed.len(), 1, "{installed:?}");
        assert_eq!(installed[0].name, "Fake & Café Scheme");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let path = sandbox.data_home().join("color-schemes/FakeScheme.colors");
    install_scheme(
        &sandbox,
        "FakeScheme.colors",
        "Fake &amp; Caf&#233; Scheme",
        &path.display().to_string(),
    );
    sandbox.run_child(
        "a_registry_name_with_entities_reads_as_the_decoded_name",
        &common::dead_store_url(),
    );
}

#[test]
fn a_registry_path_with_entities_reads_as_the_decoded_path() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config()).unwrap();
        assert_eq!(installed.len(), 1, "{installed:?}");
        assert_eq!(
            installed[0].path.file_name().unwrap(),
            Path::new("Fake&Scheme.colors").as_os_str()
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let path = sandbox.data_home().join("color-schemes/Fake&Scheme.colors");
    install_scheme(
        &sandbox,
        "Fake&Scheme.colors",
        "Fake Scheme",
        &path.display().to_string().replace('&', "&amp;"),
    );
    sandbox.run_child(
        "a_registry_path_with_entities_reads_as_the_decoded_path",
        &common::dead_store_url(),
    );
}
