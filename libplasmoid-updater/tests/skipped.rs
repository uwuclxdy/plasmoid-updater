//! An update run reports both the components it excluded and the ones it
//! could not resolve in `UpdateResult::skipped` (see `common` for the
//! sandboxed child-process harness).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{FakeStore, Listing, SCHEME_NAME, Sandbox};
use libplasmoid_updater::{Config, Reporter, update};

struct Quiet;

impl Reporter for Quiet {}

fn config(api_url: &str) -> Config {
    Config::new()
        .with_api_url(api_url)
        .with_skip_plasma_detection(true)
        .with_inhibit_idle(false)
}

const ICON_ID: u64 = 999_021;
const ICON_NAME: &str = "Lonely Icons";

#[test]
fn update_reports_an_excluded_and_an_unresolvable_component_as_skipped() {
    if let Some(child) = common::child() {
        let cfg = config(&child.store_url).with_excluded_packages(vec![SCHEME_NAME.to_string()]);
        let result = update(&cfg, &Quiet).unwrap();

        let mut skipped = result.skipped.clone();
        skipped.sort();
        let mut expected = vec![SCHEME_NAME.to_string(), ICON_NAME.to_string()];
        expected.sort();
        assert_eq!(skipped, expected, "{result:?}");
        assert!(
            result.succeeded.is_empty() && result.failed.is_empty(),
            "{result:?}"
        );
        child.done();
        return;
    }

    let sandbox = Sandbox::new();
    sandbox.install_old_scheme();
    // An icon theme the store has no entry for: its per-id lookup misses, so
    // the check cannot resolve it.
    let icons = sandbox.data_home().join("icons/Lonely-Icons");
    std::fs::create_dir_all(&icons).unwrap();
    std::fs::write(
        icons.join("index.theme"),
        "[Icon Theme]\nName=Lonely Icons\n",
    )
    .unwrap();
    std::fs::write(
        sandbox.data_home().join("knewstuff3/icons.knsregistry"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
  <stuff category="132">
    <name>{ICON_NAME}</name>
    <providerid>api.kde-look.org</providerid>
    <version>1.0</version>
    <installedfile>{installed}</installedfile>
    <id>{ICON_ID}</id>
    <status>installed</status>
  </stuff>
</hotnewstuffregistry>
"#,
            installed = icons.display()
        ),
    )
    .unwrap();

    let store = FakeStore::start(Listing::default());
    sandbox.run_child(
        "update_reports_an_excluded_and_an_unresolvable_component_as_skipped",
        store.url(),
    );
}
