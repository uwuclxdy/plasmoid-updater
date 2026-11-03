//! Store catalog handling through `check`, against the local fake KDE Store,
//! each test in its own sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{FakeStore, Listing, NEW_VERSION, SCHEME_NAME, Sandbox};
use libplasmoid_updater::{Config, check};

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

#[test]
fn check_survives_an_unparseable_catalog_page() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url), &()).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        assert_eq!(result.available_updates[0].installed.name, SCHEME_NAME);
        assert_eq!(result.available_updates[0].latest_version, NEW_VERSION);
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    // A widget with no registry entry resolves only through the catalog, so
    // the check has to read it.
    let widget = sandbox.data_home().join("plasma/plasmoids/org.fake.widget");
    std::fs::create_dir_all(&widget).unwrap();
    std::fs::write(
        widget.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Fake Widget", "Id": "org.fake.widget", "Version": "1.0"}}"#,
    )
    .unwrap();
    let store = FakeStore::start(Listing {
        catalog_garbage: true,
        ..Listing::default()
    });
    sandbox.run_child("check_survives_an_unparseable_catalog_page", store.url());
}
