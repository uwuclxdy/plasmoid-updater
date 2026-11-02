//! What a killed run strands in a component directory, and how the next
//! update treats it, each test in its own sandboxed child process (see
//! `common`).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{FakeStore, Listing, Sandbox};
use libplasmoid_updater::{Config, Reporter, update};

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
fn update_puts_back_a_widget_stranded_by_an_interrupted_swap() {
    if let Some(child) = common::child() {
        update(&config(&child.store_url), &Quiet).unwrap();
        child.done();
        return;
    }
    let sandbox = Sandbox::new();
    let plasmoids = sandbox.data_home().join("plasma/plasmoids");
    let stranded = plasmoids.join(".org.fake.widget.plasmoid-updater-old");
    write_widget(&stranded, "org.fake.widget");
    let store = FakeStore::start(Listing::default());
    sandbox.run_child(
        "update_puts_back_a_widget_stranded_by_an_interrupted_swap",
        store.url(),
    );

    let dest = plasmoids.join("org.fake.widget");
    assert!(
        dest.join("metadata.json").is_file(),
        "the stranded tree was not put back at {}",
        dest.display()
    );
    assert!(!stranded.exists());
}
