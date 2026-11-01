//! Drives the library's public entry points against a local fake KDE Store,
//! each test in its own sandboxed child process (see `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Mutex;

use common::{
    FakeStore, Listing, NEW_CONTENT, NEW_VERSION, OLD_CONTENT, OLD_VERSION, SCHEME_NAME, Sandbox,
    assert_content,
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

fn config(store_url: &str) -> Config {
    Config::new()
        .with_store_url(store_url)
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
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    let store = FakeStore::start(Listing::default());
    sandbox.run_child("install_applies_the_selection_from_check", store.url());
    assert_content(&sandbox.scheme_path(), NEW_CONTENT);
}
