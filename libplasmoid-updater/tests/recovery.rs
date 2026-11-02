//! What an interrupted earlier run leaves behind, and how the next run treats
//! it, each test in its own sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{FakeStore, Listing, NEW_CONTENT, Sandbox, assert_content};
use libplasmoid_updater::{Config, Reporter, get_installed, update};

struct Quiet;

impl Reporter for Quiet {}

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

fn write_widget(dir: &std::path::Path, id: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("metadata.json"),
        format!(r#"{{"KPlugin": {{"Name": "Fake Widget", "Id": "{id}", "Version": "1.0"}}}}"#),
    )
    .unwrap();
}

#[test]
fn get_installed_ignores_an_interrupted_installs_hidden_siblings() {
    if let Some(child) = common::child() {
        let installed = get_installed(&config(&child.store_url)).unwrap();
        let names: Vec<&str> = installed
            .iter()
            .map(|c| c.directory_name.as_str())
            .collect();
        assert_eq!(names, ["org.fake.widget"], "{installed:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    write_widget(&plasmoids.join("org.fake.widget"), "org.fake.widget");
    write_widget(
        &plasmoids.join(".org.fake.widget.plasmoid-updater-old"),
        "org.fake.widget",
    );
    write_widget(
        &plasmoids.join(".org.fake.widget.plasmoid-updater-new"),
        "org.fake.widget",
    );
    sandbox.run_child(
        "get_installed_ignores_an_interrupted_installs_hidden_siblings",
        &common::dead_store_url(),
    );
}

#[test]
fn update_removes_a_download_dir_a_killed_run_left_behind() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let stale = sandbox.tmp_dir().join("plasmoid-updater-Xk3q9z");
    std::fs::create_dir_all(stale.join("extracted")).unwrap();
    std::fs::write(stale.join("archive.tar.gz"), b"partial").unwrap();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child(
        "update_removes_a_download_dir_a_killed_run_left_behind",
        store.url(),
    );
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
    assert!(!stale.exists(), "the stale download dir survived the run");
}
