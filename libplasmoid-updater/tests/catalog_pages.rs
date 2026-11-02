//! Catalog paging through `check`, against the local fake KDE Store: the hard
//! page cap, a catalog that answered with no entries at all, a widget filed
//! only in the store's own 713 subcategory, and which requests count as the
//! store answering.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;

use common::{CatalogWidget, FakeStore, Listing, SCHEME_ID, Sandbox, TotalItems};
use libplasmoid_updater::{Config, check};

/// The hard cap on catalog pages one check may fetch.
const PAGE_CAP: usize = 64;

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

/// A widget with no registry entry, so the catalog has to be read for it.
fn add_widget(sandbox: &Sandbox) {
    let widget = sandbox.data_home().join("plasma/plasmoids/org.fake.widget");
    std::fs::create_dir_all(&widget).unwrap();
    std::fs::write(
        widget.join("metadata.json"),
        r#"{"KPlugin": {"Name": "Fake Widget", "Id": "org.fake.widget", "Version": "1.0"}}"#,
    )
    .unwrap();
}

#[test]
fn check_issues_at_most_the_page_cap() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    add_widget(&sandbox);
    let store = FakeStore::start(Listing {
        total_items: TotalItems::Items(100_000),
        ..Listing::default()
    });
    sandbox.run_child("check_issues_at_most_the_page_cap", store.url());
    let requests = store.catalog_requests();
    assert_eq!(
        requests, PAGE_CAP,
        "a totalitems of 100000 must drive exactly the cap's worth of catalog requests"
    );
}

/// A widget the store files only in its own 713 subcategory, which the 705
/// parent listing does not include.
#[test]
fn a_widget_in_the_713_subcategory_resolves_by_name() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert_eq!(result.available_updates.len(), 1, "{result:?}");
        assert_eq!(result.available_updates[0].installed.name, "Fake Widget");
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    add_widget(&sandbox);
    let store = FakeStore::start(Listing {
        widget: Some(CatalogWidget {
            id: 424_242,
            name: "Fake Widget",
            type_id: 713,
            version: "2.0",
        }),
        ..Listing::default()
    });
    sandbox.run_child(
        "a_widget_in_the_713_subcategory_resolves_by_name",
        store.url(),
    );
}

/// The catalog page cannot connect, but every per-id lookup reaches the store
/// and dies mid-body: the store answered, so the component is a diagnostic
/// rather than a network failure for the whole run.
#[test]
fn a_per_id_lookup_answered_then_cut_mid_body_counts_as_answered() {
    if let Some(child) = common::child() {
        let mut table = HashMap::new();
        table.insert("org.fake.widget".to_string(), SCHEME_ID);
        let result = check(&config(&child.store_url).with_widgets_id_table(table))
            .expect("an answered per-id lookup is not a network failure");
        let diagnostic = result
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.name == "Fake Widget")
            .expect("the answered lookup degrades to a diagnostic");
        assert_eq!(
            diagnostic.reason,
            format!("could not fetch store entry {SCHEME_ID}")
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    add_widget(&sandbox);
    let store = FakeStore::start(Listing {
        in_catalog: false,
        catalog_connects_nowhere: true,
        per_id_drops_mid_body: true,
        ..Listing::default()
    });
    sandbox.run_child(
        "a_per_id_lookup_answered_then_cut_mid_body_counts_as_answered",
        store.url(),
    );
}

#[test]
fn a_failed_targeted_fetch_names_the_id_it_could_not_fetch() {
    if let Some(child) = common::child() {
        let mut table = HashMap::new();
        table.insert("org.fake.widget".to_string(), SCHEME_ID);
        let result = check(&config(&child.store_url).with_widgets_id_table(table)).unwrap();
        let widget = result
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.name == "Fake Widget")
            .expect("the widget's failed lookup is a diagnostic");
        assert_eq!(
            widget.reason,
            format!("could not fetch store entry {SCHEME_ID}")
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    add_widget(&sandbox);
    let store = FakeStore::start(Listing {
        in_catalog: false,
        per_id_fails: true,
        ..Listing::default()
    });
    sandbox.run_child(
        "a_failed_targeted_fetch_names_the_id_it_could_not_fetch",
        store.url(),
    );
}

#[test]
fn check_survives_an_answered_catalog_with_no_entries() {
    if let Some(child) = common::child() {
        let result = check(&config(&child.store_url)).unwrap();
        assert!(result.available_updates.is_empty(), "{result:?}");
        let reasons: Vec<&str> = result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.reason.as_str())
            .collect();
        assert_eq!(
            reasons,
            vec![
                "could not match to kde store entry",
                "failed to fetch store entry"
            ],
            "{result:?}"
        );
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    add_widget(&sandbox);
    let store = FakeStore::start(Listing {
        in_catalog: false,
        per_id_connects_nowhere: true,
        ..Listing::default()
    });
    sandbox.run_child(
        "check_survives_an_answered_catalog_with_no_entries",
        store.url(),
    );
}
