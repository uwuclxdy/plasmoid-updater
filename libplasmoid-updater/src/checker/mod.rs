// SPDX-License-Identifier: GPL-3.0-or-later

mod discovery;
mod evaluation;
mod registry;
mod resolution;
mod store;

use std::collections::HashMap;

use crate::{
    Result,
    api::ApiClient,
    config::Config,
    types::{Diagnostic, UpdateCheckResult},
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

pub(crate) use discovery::{find_installed, read_metadata};

/// Pre-built lookup tables for resolving component content IDs.
///
/// Bundles the two hash maps that are threaded through every check function,
/// reducing parameter count across the checker module.
pub(crate) struct IdLookup<'a> {
    pub widgets_id_table: &'a HashMap<String, u64>,
    pub registry_id_cache: &'a HashMap<String, u64>,
}

/// Checks for updates using pre-discovered components.
pub(crate) fn check_with_components(
    config: &Config,
    api_client: &ApiClient,
    components: Vec<crate::types::InstalledComponent>,
) -> Result<UpdateCheckResult> {
    let mut result = UpdateCheckResult::default();
    add_stranded_diagnostics(config.system, &mut result);

    if components.is_empty() {
        return Ok(result);
    }

    let (registry_components, regular_components) = store::partition_components(components);

    // Build local caches before any network call so fetch_store_entries
    // can resolve known IDs without touching the paginated catalog.
    let registry_id_cache = crate::registry::build_id_cache(config.system);

    let lookup = IdLookup {
        widgets_id_table: &config.widgets_id_table,
        registry_id_cache: &registry_id_cache,
    };

    let store = store::fetch_store_entries(api_client, &regular_components, &lookup)?;

    let regular_results: Vec<evaluation::ComponentCheckResult> = regular_components
        .par_iter()
        .map(|component| evaluation::check_component(component, &store.entries, &lookup))
        .collect();

    for check_result in regular_results {
        match check_result {
            evaluation::ComponentCheckResult::Update(update) => result.add_update(*update),
            evaluation::ComponentCheckResult::Unresolved(diagnostic) => {
                result.add_unresolved(diagnostic);
            }
            evaluation::ComponentCheckResult::NoMatchingDownload {
                component,
                diagnostic,
            } => {
                result.add_no_matching_download(component, diagnostic);
            }
            evaluation::ComponentCheckResult::UpToDate => {}
        }
    }

    registry::check_components(
        &registry_components,
        api_client,
        &store.entries,
        store.answered,
        &lookup,
        &mut result,
    )?;

    Ok(result)
}

/// Reports every component a killed run stranded in a hidden sibling. `check`
/// never writes, so the diagnostic is the only thing it can offer; `update`
/// repairs the tree first, before this check runs.
///
/// A hidden sibling seen while another run holds the update lock belongs to
/// that live run's swap, not to a dead one, so the scan is skipped then.
fn add_stranded_diagnostics(system: bool, result: &mut UpdateCheckResult) {
    if crate::installer::run_lock_held() {
        return;
    }
    for name in crate::installer::stranded_components(system) {
        result.add_unresolved(Diagnostic::new(
            name,
            "interrupted install, run update to restore".to_string(),
        ));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::{IdLookup, resolution};
    use crate::types::{ComponentType, InstalledComponent};

    const CHILD: &str = "PLASMOID_UPDATER_ID_CACHE_TEST_CHILD";
    const ROOT: &str = "PLASMOID_UPDATER_ID_CACHE_TEST_ROOT";
    const PLANTED_ID: u64 = 2_380_745;
    const USER_ID: u64 = 111_001;
    const TABLE_ID: u64 = 777_001;
    const WIDGET: &str = "org.frapell.networkspeed";
    const USER_WIDGET: &str = "user-widget";

    fn component(directory_name: &str, is_system: bool) -> InstalledComponent {
        InstalledComponent {
            name: directory_name.to_string(),
            directory_name: directory_name.to_string(),
            version: "1.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path: PathBuf::from(format!("/plasmoids/{directory_name}")),
            is_system,
            release_date: String::new(),
        }
    }

    /// Under `--system` the store id a system component resolves to may come
    /// only from a trusted system registry: a user-writable registry must not
    /// choose the content a privileged update installs. A user run still reads
    /// the user registry.
    const STRANDED_LOCK_CHILD: &str = "PLASMOID_UPDATER_STRANDED_LOCK_CHILD";
    const STRANDED_LOCK_MARKER: &str = "PLASMOID_UPDATER_STRANDED_LOCK_MARKER";

    /// A hidden sibling seen while another process holds the run lock belongs
    /// to that live run, not to a dead one: it must not be reported.
    #[test]
    fn a_live_run_lock_hides_its_swap_from_the_stranded_scan() {
        if std::env::var_os(STRANDED_LOCK_CHILD).is_some() {
            let data = PathBuf::from(std::env::var("XDG_DATA_HOME").unwrap());
            let stranded = data
                .join("plasma/plasmoids")
                .join(".org.fake.widget.plasmoid-updater-old");
            std::fs::create_dir_all(&stranded).unwrap();
            std::fs::write(
                stranded.join("metadata.json"),
                r#"{"KPlugin": {"Id": "org.fake.widget", "Name": "Fake Widget"}}"#,
            )
            .unwrap();

            let mut result = crate::types::UpdateCheckResult::default();
            let lock = crate::installer::UpdateLock::acquire().unwrap();
            super::add_stranded_diagnostics(false, &mut result);
            assert!(
                result.unresolved.is_empty(),
                "a live run's lock did not hide its swap: {:?}",
                result.unresolved
            );
            drop(lock);
            super::add_stranded_diagnostics(false, &mut result);
            assert_eq!(
                result.unresolved.len(),
                1,
                "the scan dropped a real stranded tree: {:?}",
                result.unresolved
            );
            std::fs::write(std::env::var(STRANDED_LOCK_MARKER).unwrap(), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for name in ["home", "data", "cache", "config", "tmp"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let marker = root.join("child-done");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "checker::tests::a_live_run_lock_hides_its_swap_from_the_stranded_scan",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("TMPDIR", root.join("tmp"))
            .env(STRANDED_LOCK_CHILD, "1")
            .env(STRANDED_LOCK_MARKER, &marker)
            .output()
            .unwrap();
        assert!(
            output.status.success() && marker.exists(),
            "child failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            marker.exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn a_user_registry_entry_does_not_resolve_a_system_component() {
        if std::env::var_os(CHILD).is_some() {
            let root = PathBuf::from(std::env::var(ROOT).unwrap());
            let mut table = HashMap::new();
            table.insert(WIDGET.to_string(), TABLE_ID);

            let system_cache = crate::registry::build_id_cache(true);
            let system_lookup = IdLookup {
                widgets_id_table: &table,
                registry_id_cache: &system_cache,
            };
            assert_eq!(
                resolution::resolve_content_id(&component(WIDGET, true), &[], &system_lookup),
                Some(TABLE_ID),
                "the user registry chose a system component's store content: {system_cache:?}"
            );

            let user_cache = crate::registry::build_id_cache(false);
            let user_lookup = IdLookup {
                widgets_id_table: &table,
                registry_id_cache: &user_cache,
            };
            assert_eq!(
                resolution::resolve_content_id(&component(USER_WIDGET, false), &[], &user_lookup),
                Some(USER_ID),
                "a user component no longer resolves through the user registry: {user_cache:?}"
            );

            std::fs::write(root.join("child-done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let registry_dir = root.join("data/knewstuff3");
        std::fs::create_dir_all(&registry_dir).unwrap();
        std::fs::write(
            registry_dir.join("plasmoids.knsregistry"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="705">
    <name>Network Speed</name>
    <version>1.0</version>
    <installedfile>/usr/share/plasma/plasmoids/{WIDGET}/metadata.json</installedfile>
    <id>{PLANTED_ID}</id>
  </stuff>
  <stuff category="705">
    <name>User Widget</name>
    <version>1.0</version>
    <installedfile>{user}/metadata.json</installedfile>
    <id>{USER_ID}</id>
  </stuff>
</hotnewstuffregistry>
"#,
                user = root
                    .join("data/plasma/plasmoids")
                    .join(USER_WIDGET)
                    .display(),
            ),
        )
        .unwrap();
        let system = root.join("system");
        std::fs::create_dir_all(&system).unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "checker::tests::a_user_registry_entry_does_not_resolve_a_system_component",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_DIRS", &system)
            .env("TMPDIR", root.join("tmp"))
            .env(CHILD, "1")
            .env(ROOT, &root)
            .output()
            .unwrap();

        assert!(
            output.status.success() && root.join("child-done").exists(),
            "child failed ({}; body completed: {})\n--- stdout\n{}\n--- stderr\n{}",
            output.status,
            root.join("child-done").exists(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
