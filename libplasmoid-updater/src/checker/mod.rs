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
    registry::{RegistryIdCache, RegistryInstall},
    types::{Diagnostic, InstalledComponent, UpdateCheckResult},
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

pub(crate) use discovery::{find_installed, read_metadata};

/// Pre-built lookup tables for resolving component content IDs.
///
/// Bundles the two hash maps that are threaded through every check function,
/// reducing parameter count across the checker module.
pub(crate) struct IdLookup<'a> {
    pub widgets_id_table: &'a HashMap<String, u64>,
    pub registry_id_cache: &'a RegistryIdCache,
}

impl IdLookup<'_> {
    /// The store id the registry recorded for `component`'s own type and
    /// install root: a same-named component of another type never answers.
    pub(crate) fn registry_id(&self, component: &InstalledComponent) -> Option<u64> {
        self.registry_id_cache
            .get(&RegistryInstall {
                component_type: component.component_type,
                path: crate::registry::normalize_path(&component.path),
            })
            .copied()
    }
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

    let regular_results: Vec<(
        &crate::types::InstalledComponent,
        evaluation::ComponentCheckResult,
    )> = regular_components
        .par_iter()
        .map(|component| {
            (
                component,
                evaluation::check_component(component, &store.entries, &lookup),
            )
        })
        .collect();

    for (component, check_result) in regular_results {
        record_component_check(component, check_result, &mut result);
    }

    registry::check_components(
        &registry_components,
        api_client,
        &store.entries,
        store.answered,
        &lookup,
        config.system,
        &mut result,
    )?;

    Ok(result)
}

/// Routes one component's check outcome into the result. A system component no
/// store entry matches is skipped, not failed; every other outcome keeps the
/// user-run shape.
fn record_component_check(
    component: &crate::types::InstalledComponent,
    check: evaluation::ComponentCheckResult,
    result: &mut UpdateCheckResult,
) {
    match check {
        evaluation::ComponentCheckResult::Update(update) => result.add_update(*update),
        evaluation::ComponentCheckResult::Unmatched(diagnostic)
        | evaluation::ComponentCheckResult::TypeMismatch(diagnostic) => {
            if component.is_system {
                result.skipped.push(diagnostic.name);
            } else {
                result.add_unresolved(diagnostic);
            }
        }
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

    use super::{IdLookup, evaluation, record_component_check, resolution};
    use crate::types::{ComponentType, DownloadLink, InstalledComponent, StoreEntry};

    const CHILD: &str = "PLASMOID_UPDATER_ID_CACHE_TEST_CHILD";
    const ROOT: &str = "PLASMOID_UPDATER_ID_CACHE_TEST_ROOT";
    const PLANTED_ID: u64 = 2_380_745;
    const USER_ID: u64 = 111_001;
    const TABLE_ID: u64 = 777_001;
    const WIDGET: &str = "org.frapell.networkspeed";
    const USER_WIDGET: &str = "user-widget";

    fn component(path: PathBuf, is_system: bool) -> InstalledComponent {
        let directory_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        InstalledComponent {
            name: directory_name.clone(),
            directory_name,
            version: "1.0".to_string(),
            component_type: ComponentType::PlasmaWidget,
            path,
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
                resolution::resolve_content_id(
                    &component(
                        PathBuf::from("/usr/share/plasma/plasmoids").join(WIDGET),
                        true,
                    ),
                    &[],
                    &system_lookup,
                ),
                Some(TABLE_ID),
                "the user registry chose a system component's store content: {system_cache:?}"
            );

            let user_cache = crate::registry::build_id_cache(false);
            let user_lookup = IdLookup {
                widgets_id_table: &table,
                registry_id_cache: &user_cache,
            };
            let user_widget = root.join("data/plasma/plasmoids").join(USER_WIDGET);
            assert_eq!(
                resolution::resolve_content_id(&component(user_widget, false), &[], &user_lookup),
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

    /// A `--system` run skips a component no store entry matches instead of
    /// failing it; a user run keeps it unresolved.
    #[test]
    fn an_unmatched_system_widget_is_skipped_not_failed() {
        let component = component(PathBuf::from("/plasmoids/org.example.distro"), true);
        let diagnostic = crate::types::Diagnostic::new(
            component.name.clone(),
            "could not match to kde store entry".to_string(),
        );
        let mut result = crate::types::UpdateCheckResult::default();

        record_component_check(
            &component,
            evaluation::ComponentCheckResult::Unmatched(diagnostic),
            &mut result,
        );

        assert_eq!(result.skipped, vec![component.name]);
        assert!(
            result.unresolved.is_empty() && result.check_failures.is_empty(),
            "{result:?}"
        );
    }

    #[test]
    fn an_unmatched_user_widget_is_still_unresolved() {
        let component = component(PathBuf::from("/plasmoids/org.example.user"), false);
        let diagnostic = crate::types::Diagnostic::new(
            component.name.clone(),
            "could not match to kde store entry".to_string(),
        );
        let mut result = crate::types::UpdateCheckResult::default();

        record_component_check(
            &component,
            evaluation::ComponentCheckResult::Unmatched(diagnostic),
            &mut result,
        );

        assert!(result.skipped.is_empty(), "{result:?}");
        assert_eq!(result.unresolved.len(), 1);
    }

    /// A store entry of a type no component under test can be: an icon theme,
    /// foreign to both a plasma widget and a color scheme.
    fn foreign_store_entry(id: u64) -> StoreEntry {
        StoreEntry {
            id,
            name: "Foreign".to_string(),
            author: String::new(),
            version: "2.0".to_string(),
            type_id: 132,
            download_links: vec![DownloadLink {
                url: "https://example.test/dl".to_string(),
                version: "2.0".to_string(),
                checksum: None,
                size_kb: None,
            }],
            changed_date: "2025-06-01".to_string(),
        }
    }

    /// A `--system` widget whose store id resolves to an entry of another
    /// component type is skipped, not failed: that entry is no entry of its
    /// own type.
    #[test]
    fn a_system_widget_with_a_wrong_type_store_entry_is_skipped() {
        let component = component(PathBuf::from("/plasmoids/org.example.widget"), true);
        let mut result = crate::types::UpdateCheckResult::default();

        let check = evaluation::evaluate_store_entry(&component, &foreign_store_entry(123), 123);
        record_component_check(&component, check, &mut result);

        assert_eq!(result.skipped, vec![component.name]);
        assert!(
            result.unresolved.is_empty() && result.check_failures.is_empty(),
            "{result:?}"
        );
    }

    /// A user-run widget with a wrong-type store entry keeps today's routing:
    /// it stays unresolved.
    #[test]
    fn a_user_widget_with_a_wrong_type_store_entry_stays_unresolved() {
        let component = component(PathBuf::from("/plasmoids/org.example.user"), false);
        let mut result = crate::types::UpdateCheckResult::default();

        let check = evaluation::evaluate_store_entry(&component, &foreign_store_entry(123), 123);
        record_component_check(&component, check, &mut result);

        assert!(result.skipped.is_empty(), "{result:?}");
        assert_eq!(result.unresolved.len(), 1);
    }

    /// A `--system` registry component whose resolved id names a wrong-type
    /// store entry is skipped, not failed, through the registry path.
    #[test]
    fn a_system_registry_component_with_a_wrong_type_store_entry_is_skipped() {
        use crate::{Config, api::ApiClient};

        let config = Config::new().with_api_url("http://127.0.0.1:1/ocs/v1");
        let client = ApiClient::new(&config).unwrap();
        let mut component = component(PathBuf::from("/plasmoids/org.example.scheme"), true);
        component.component_type = ComponentType::ColorScheme;

        let wid = HashMap::new();
        let mut reg = HashMap::new();
        reg.insert(
            crate::registry::RegistryInstall {
                component_type: ComponentType::ColorScheme,
                path: crate::registry::normalize_path(&component.path),
            },
            123,
        );
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };
        let mut result = crate::types::UpdateCheckResult::default();

        super::registry::check_components(
            std::slice::from_ref(&component),
            &client,
            &[foreign_store_entry(123)],
            true,
            &lookup,
            true,
            &mut result,
        )
        .unwrap();

        assert_eq!(result.skipped, vec![component.name]);
        assert!(
            result.unresolved.is_empty() && result.check_failures.is_empty(),
            "{result:?}"
        );
    }

    /// A `--system` check over a regular component no store entry matches skips
    /// it instead of failing it, through the production `check_with_components`
    /// entry that routes the `Unmatched` outcome. The check runs in a child
    /// process with fresh XDG dirs and the run lock held, so neither the
    /// stranded scan nor the id cache reads the host's real system data dirs.
    #[test]
    fn a_system_check_skips_an_unmatched_regular_component_through_the_entry() {
        use std::io::{BufRead as _, Write as _};
        use std::net::TcpListener;

        use crate::{Config, api::ApiClient};

        const CHILD: &str = "PLASMOID_UPDATER_CHECK_ENTRY_CHILD";
        const ROOT: &str = "PLASMOID_UPDATER_CHECK_ENTRY_ROOT";

        if std::env::var_os(CHILD).is_some() {
            let root = PathBuf::from(std::env::var(ROOT).unwrap());

            // Serve an empty catalog, so the store answers without matching the
            // component and no per-id lookup runs.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if reader.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" {
                            break;
                        }
                    }
                    let body = "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>100</statuscode><message></message><totalitems>0</totalitems><itemsperpage>100</itemsperpage></meta><data></data></ocs>";
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body.as_bytes());
                }
            });

            // Hold the run lock so the stranded scan (which reads the host's
            // `/usr/share`) is skipped; the routing this test pins is unaffected.
            let _lock = crate::installer::UpdateLock::acquire().unwrap();

            let config = Config::new()
                .with_system(true)
                .with_api_url(format!("http://127.0.0.1:{port}/ocs/v1"));
            let api_client = ApiClient::new(&config).unwrap();
            let component = component(PathBuf::from("/plasmoids/org.example.distro"), true);

            let result =
                super::check_with_components(&config, &api_client, vec![component.clone()])
                    .unwrap();

            assert_eq!(result.skipped, vec![component.name]);
            assert!(
                result.unresolved.is_empty() && result.check_failures.is_empty(),
                "{result:?}"
            );
            std::fs::write(root.join("child-done"), b"ok").unwrap();
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for name in [
            "home",
            "data",
            "data-dirs",
            "cache",
            "config",
            "runtime",
            "tmp",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "checker::tests::a_system_check_skips_an_unmatched_regular_component_through_the_entry",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_DATA_DIRS", root.join("data-dirs"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
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

    /// A `--system` check over a registry-only component no store entry matches
    /// skips it instead of failing it, through the registry path that routes
    /// the unresolved-id case.
    #[test]
    fn a_system_check_skips_an_unmatched_registry_component() {
        use crate::{Config, api::ApiClient};

        let config = Config::new().with_api_url("http://127.0.0.1:1/ocs/v1");
        let client = ApiClient::new(&config).unwrap();
        let mut component = component(PathBuf::from("/plasmoids/org.example.distro"), true);
        component.component_type = ComponentType::ColorScheme;

        let wid = HashMap::new();
        let reg = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &wid,
            registry_id_cache: &reg,
        };
        let mut result = crate::types::UpdateCheckResult::default();

        super::registry::check_components(
            std::slice::from_ref(&component),
            &client,
            &[],
            true,
            &lookup,
            true,
            &mut result,
        )
        .unwrap();

        assert_eq!(result.skipped, vec![component.name]);
        assert!(
            result.unresolved.is_empty() && result.check_failures.is_empty(),
            "{result:?}"
        );
    }
}
