//! A failed KNewStuff registry rewrite is a component failure, not a silent
//! success (see `common` for the sandboxed child-process harness).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt as _;

use common::{FakeStore, Listing, NEW_CONTENT, OLD_VERSION, Sandbox, assert_content};
use libplasmoid_updater::{Config, FailureKind, Reporter, update};

struct Quiet;

impl Reporter for Quiet {}

fn config() -> Config {
    Config::new()
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

#[test]
fn a_registry_write_failure_is_reported_and_leaves_the_registry_intact() {
    if let Some(child) = common::child() {
        let result = update(&config().with_api_url(child.store_url.clone()), &Quiet).unwrap();
        assert!(result.succeeded.is_empty(), "{result:?}");
        assert_eq!(result.failed.len(), 1, "{result:?}");
        assert_eq!(result.failed[0].kind, FailureKind::Registry, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());

    // Read-only registry directory: the write's temp file cannot be created,
    // while the installed file itself stays writable.
    let registry_dir = sandbox.registry_path().parent().unwrap().to_path_buf();
    set_mode(&registry_dir, 0o555);
    sandbox.run_child(
        "a_registry_write_failure_is_reported_and_leaves_the_registry_intact",
        store.url(),
    );
    set_mode(&registry_dir, 0o755);

    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
    assert!(
        sandbox
            .registry_content()
            .contains(&version_line(OLD_VERSION)),
        "the registry was not left intact:\n{}",
        sandbox.registry_content()
    );
}

fn version_line(version: &str) -> String {
    format!("<version>{version}</version>")
}

fn set_mode(path: &std::path::Path, mode: u32) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(mode);
    std::fs::set_permissions(path, permissions).unwrap();
}
