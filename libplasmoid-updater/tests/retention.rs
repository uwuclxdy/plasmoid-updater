//! Backup retention across one `update` run, in a sandboxed child process
//! (see `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use common::{FakeStore, Listing, NEW_CONTENT, OLD_CONTENT, SCHEME_FILE, Sandbox, assert_content};
use libplasmoid_updater::{Config, Reporter, update};

struct Quiet;

impl Reporter for Quiet {}

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

/// The sandbox's cache home, the sibling of the `TMPDIR` it exposes.
fn cache_home(sandbox: &Sandbox) -> PathBuf {
    sandbox.tmp_dir().parent().unwrap().join("cache")
}

fn backup_root(sandbox: &Sandbox) -> PathBuf {
    cache_home(sandbox).join("plasmoid-updater/backups")
}

/// An earlier run's backup of the fake color scheme.
fn plant_backup(root: &Path, timestamp: &str) {
    let backup = root.join(timestamp).join("color-schemes");
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join(SCHEME_FILE), format!("# backup {timestamp}\n")).unwrap();
}

/// More backups than the keep count, all older than the run under test.
const PLANTED: [&str; 6] = [
    "2020-01-01T00-00-00",
    "2020-01-02T00-00-00",
    "2020-01-03T00-00-00",
    "2020-01-04T00-00-00",
    "2020-01-05T00-00-00",
    "2020-01-06T00-00-00",
];

#[test]
fn an_update_prunes_old_backups_and_keeps_its_own() {
    if let Some(child) = common::child() {
        let result = update(&config(&child.store_url), &Quiet).unwrap();
        assert_eq!(result.succeeded.len(), 1, "{result:?}");
        child.done();
        return;
    }

    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let backups = backup_root(&sandbox);
    for timestamp in PLANTED {
        plant_backup(&backups, timestamp);
    }

    let store = FakeStore::start(Listing::default());
    sandbox.run_child(
        "an_update_prunes_old_backups_and_keeps_its_own",
        store.url(),
    );

    assert_content(&sandbox.scheme_path(), NEW_CONTENT);

    let remaining: Vec<String> = std::fs::read_dir(&backups)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(remaining.len(), 5, "kept backups: {remaining:?}");
    assert!(
        !remaining.contains(&PLANTED[0].to_string()),
        "the oldest backup survived: {remaining:?}"
    );
    assert!(
        !remaining.contains(&PLANTED[1].to_string()),
        "the second oldest backup survived: {remaining:?}"
    );
    for kept in &PLANTED[2..] {
        assert!(
            remaining.contains(&kept.to_string()),
            "{kept} was pruned: {remaining:?}"
        );
    }

    let own = remaining
        .iter()
        .find(|name| !PLANTED.contains(&name.as_str()))
        .unwrap_or_else(|| panic!("this run's backup was pruned: {remaining:?}"));
    let owned_scheme = backups.join(own).join("color-schemes").join(SCHEME_FILE);
    assert_eq!(
        std::fs::read_to_string(&owned_scheme).unwrap(),
        OLD_CONTENT,
        "this run's backup does not hold the replaced scheme"
    );
}
