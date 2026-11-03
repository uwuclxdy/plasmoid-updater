// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};

use crate::{
    Error,
    api::ApiClient,
    types::{Diagnostic, InstalledComponent, StoreEntry, UpdateCheckResult},
};

use super::{IdLookup, evaluation, resolution};

/// Checks if any of the components from the widget-id registry table have updates available.
pub(crate) fn check_components(
    registry_components: &[InstalledComponent],
    client: &ApiClient,
    store_entries: &[StoreEntry],
    store_answered: bool,
    lookup: &IdLookup,
    system: bool,
    result: &mut UpdateCheckResult,
) -> crate::Result<()> {
    let resolutions: Vec<(&InstalledComponent, Option<u64>)> = registry_components
        .iter()
        .map(|c| (c, resolution::resolve_content_id(c, store_entries, lookup)))
        .collect();

    let resolved: Vec<(&InstalledComponent, u64)> = resolutions
        .iter()
        .filter_map(|(component, id)| (*id).map(|id| (*component, id)))
        .collect();

    // Reuse any entries already present in store_entries; fetch only the rest.
    let missing_ids: Vec<u64> = {
        let mut seen = HashSet::new();
        resolved
            .iter()
            .filter(|(_, id)| resolution::find_store_entry(store_entries, *id).is_none())
            .filter(|(_, id)| seen.insert(*id))
            .map(|(_, id)| *id)
            .collect()
    };

    // A per-id lookup failure (a gone entry, a bad status, a timeout, a
    // dropped connection) degrades that component to a diagnostic below.
    // Only a lookup that could not connect to the store at all proves the
    // store unreachable, and only when the store answered no request (catalog
    // or per-id) and no store entry is available from anywhere else — then
    // the run fails. An empty catalog page is an answer, so a store that
    // reported zero entries has been reached and its lookups stay diagnostics.
    let mut fetched: HashMap<u64, StoreEntry> = HashMap::new();
    let mut connect_failures = 0usize;
    let mut last_connect_error: Option<Error> = None;
    for (id, detail) in missing_ids.iter().zip(client.fetch_details(&missing_ids)) {
        match detail.result {
            Ok(entry) => {
                fetched.insert(*id, entry);
            }
            Err(e) => {
                log::warn!(target: "resolver", "store entry {id} unavailable: {e}");
                if matches!(&e, Error::Network(err) if err.is_connect()) {
                    connect_failures += 1;
                    last_connect_error = Some(e);
                }
            }
        }
    }

    if let Some(err) = last_connect_error
        && connect_failures == missing_ids.len()
        && fetched.is_empty()
        && !store_answered
    {
        return Err(err);
    }

    for (component, content_id) in &resolved {
        let entry = resolution::find_store_entry(store_entries, *content_id)
            .or_else(|| fetched.get(content_id));

        match entry {
            Some(entry) => match evaluation::evaluate_store_entry(component, entry, *content_id) {
                evaluation::ComponentCheckResult::Update(update) => {
                    result.add_update(*update);
                }
                evaluation::ComponentCheckResult::NoMatchingDownload {
                    component,
                    diagnostic,
                } => {
                    result.add_no_matching_download(component, diagnostic);
                }
                evaluation::ComponentCheckResult::UpToDate => {}
                evaluation::ComponentCheckResult::Unmatched(diagnostic) => {
                    if system {
                        result.skipped.push(diagnostic.name);
                    } else {
                        result.add_unresolved(diagnostic);
                    }
                }
                evaluation::ComponentCheckResult::TypeMismatch(diagnostic) => {
                    if system {
                        result.skipped.push(diagnostic.name);
                    } else {
                        result.add_check_failure(diagnostic);
                    }
                }
                evaluation::ComponentCheckResult::Unresolved(diagnostic) => {
                    result.add_check_failure(diagnostic);
                }
            },
            None => {
                let diagnostic = Diagnostic::new(
                    component.name.clone(),
                    "failed to fetch store entry".to_string(),
                )
                .with_content_id(*content_id);
                result.add_check_failure(diagnostic);
            }
        }
    }

    for (component, resolved) in &resolutions {
        if resolved.is_some() {
            continue;
        }
        // A system component no store entry matches is skipped, not failed: a
        // distro package the store never shipped must not fail `--system check`.
        if system {
            result.skipped.push(component.name.clone());
            continue;
        }
        let diagnostic = Diagnostic::new(
            component.name.clone(),
            "could not match to kde store entry".to_string(),
        );
        result.add_unresolved(diagnostic);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::config::Config;
    use crate::registry::{RegistryIdCache, RegistryInstall};
    use crate::types::{ComponentType, DownloadLink};

    const COLOR_SCHEME_TYPE: u16 = 112;

    fn scheme(version: &str) -> InstalledComponent {
        let path = PathBuf::from("/usr/share/color-schemes/FakeScheme.colors");
        InstalledComponent {
            name: "Fake Scheme".to_string(),
            directory_name: "FakeScheme.colors".to_string(),
            version: version.to_string(),
            component_type: ComponentType::ColorScheme,
            path,
            is_system: true,
            release_date: String::new(),
        }
    }

    fn store_entry(id: u64, type_id: u16, version: &str) -> StoreEntry {
        StoreEntry {
            id,
            name: "Other Scheme".to_string(),
            author: String::new(),
            version: version.to_string(),
            type_id,
            download_links: vec![DownloadLink {
                url: format!("https://example.test/{id}.tar.gz"),
                version: version.to_string(),
                checksum: None,
                size_kb: None,
            }],
            changed_date: "2026-10-01".to_string(),
        }
    }

    fn install(component: &InstalledComponent) -> RegistryInstall {
        RegistryInstall {
            component_type: component.component_type,
            path: crate::registry::normalize_path(&component.path),
        }
    }

    fn dead_client() -> ApiClient {
        ApiClient::new(&Config::new().with_api_url("http://127.0.0.1:0/ocs/v1")).unwrap()
    }

    /// A store that redirects the lookup for id 9 to port 0, which no process
    /// can own, and answers the lookup for id 7 with a 404: one lookup cannot
    /// connect while its sibling reached the store and got an answer back.
    fn one_refused_one_answered_client() -> ApiClient {
        let (base, _requests) = crate::testutil::serve(|target| {
            if target.contains("/content/data/7") {
                crate::testutil::not_found()
            } else {
                crate::testutil::redirect("http://127.0.0.1:0/nope")
            }
        });
        ApiClient::new(&Config::new().with_api_url(base)).unwrap()
    }

    fn check(
        components: &[InstalledComponent],
        store_entries: &[StoreEntry],
        store_answered: bool,
        lookup: &IdLookup<'_>,
        system: bool,
        client: &ApiClient,
    ) -> crate::Result<UpdateCheckResult> {
        let mut result = UpdateCheckResult::default();
        check_components(
            components,
            client,
            store_entries,
            store_answered,
            lookup,
            system,
            &mut result,
        )?;
        Ok(result)
    }

    /// A registry-recorded id whose entry is already in hand is evaluated from
    /// that entry, with no request to the store.
    #[test]
    fn a_registry_component_uses_its_store_entry_without_fetching() {
        let component = scheme("1.0");
        let entry = store_entry(5, COLOR_SCHEME_TYPE, "2.0");
        let mut registry = RegistryIdCache::new();
        registry.insert(install(&component), 5);
        let widgets = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        let result = check(
            std::slice::from_ref(&component),
            std::slice::from_ref(&entry),
            false,
            &lookup,
            false,
            &dead_client(),
        )
        .unwrap();

        assert_eq!(result.updates.len(), 1);
        assert_eq!(result.updates[0].content_id, 5);
        assert_eq!(result.updates[0].latest_version, "2.0");
        assert!(
            result.unresolved.is_empty()
                && result.check_failures.is_empty()
                && result.skipped.is_empty()
        );
    }

    /// The entry a resolved id names belongs to another component type: a
    /// `--system` run skips the component, a user run reports the check failed.
    #[test]
    fn a_system_type_mismatched_entry_is_skipped_and_a_user_one_fails_the_check() {
        let component = scheme("1.0");
        let entry = store_entry(5, 132, "2.0");
        let mut registry = RegistryIdCache::new();
        registry.insert(install(&component), 5);
        let widgets = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        let system = check(
            std::slice::from_ref(&component),
            std::slice::from_ref(&entry),
            false,
            &lookup,
            true,
            &dead_client(),
        )
        .unwrap();
        let user = check(
            std::slice::from_ref(&component),
            std::slice::from_ref(&entry),
            false,
            &lookup,
            false,
            &dead_client(),
        )
        .unwrap();

        assert_eq!(system.skipped, vec![component.name.clone()]);
        assert!(system.check_failures.is_empty() && system.unresolved.is_empty());
        assert!(user.skipped.is_empty() && user.unresolved.is_empty());
        assert_eq!(user.check_failures.len(), 1);
        assert_eq!(
            user.check_failures[0].reason,
            "store entry 5 is a different component type"
        );
    }

    /// A component no lookup resolves to a store id: a `--system` run skips it,
    /// a user run reports it unresolved.
    #[test]
    fn a_system_component_with_no_store_id_is_skipped_and_a_user_one_is_unresolved() {
        let component = scheme("1.0");
        let widgets = HashMap::new();
        let registry = RegistryIdCache::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        let system = check(
            std::slice::from_ref(&component),
            &[],
            false,
            &lookup,
            true,
            &dead_client(),
        )
        .unwrap();
        let user = check(
            std::slice::from_ref(&component),
            &[],
            false,
            &lookup,
            false,
            &dead_client(),
        )
        .unwrap();

        assert_eq!(system.skipped, vec![component.name.clone()]);
        assert!(system.unresolved.is_empty());
        assert!(user.skipped.is_empty());
        assert_eq!(user.unresolved.len(), 1);
        assert_eq!(
            user.unresolved[0].reason,
            "could not match to kde store entry"
        );
    }

    /// Every lookup failed to connect and the store answered nothing else: the
    /// store is unreachable, so the check fails rather than blaming components.
    #[test]
    fn a_store_that_answered_nothing_and_failed_every_lookup_fails_the_check() {
        let component = scheme("1.0");
        let mut widgets = HashMap::new();
        widgets.insert(component.directory_name.clone(), 9);
        let registry = RegistryIdCache::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        let error = check(
            std::slice::from_ref(&component),
            &[],
            false,
            &lookup,
            false,
            &dead_client(),
        )
        .unwrap_err();

        assert!(matches!(error, Error::Network(_)), "{error:?}");
    }

    /// One lookup cannot connect while a sibling lookup reaches the store and
    /// is answered: the store is reachable, so even though every missing id
    /// failed, the check reports both components instead of failing the run.
    #[test]
    fn a_connect_failure_beside_an_answered_lookup_is_not_an_unreachable_store() {
        let refused = scheme("1.0");
        let mut answered = scheme("1.0");
        answered.name = "Other Scheme".to_string();
        answered.directory_name = "OtherScheme.colors".to_string();
        let mut widgets = HashMap::new();
        widgets.insert(refused.directory_name.clone(), 9);
        widgets.insert(answered.directory_name.clone(), 7);
        let registry = RegistryIdCache::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        let result = check(
            &[refused, answered],
            &[],
            false,
            &lookup,
            false,
            &one_refused_one_answered_client(),
        )
        .unwrap();

        assert_eq!(result.check_failures.len(), 2);
        assert_eq!(
            result.check_failures[0].reason,
            "failed to fetch store entry"
        );
        assert_eq!(result.check_failures[0].content_id, Some(9));
        assert_eq!(
            result.check_failures[1].reason,
            "failed to fetch store entry"
        );
        assert_eq!(result.check_failures[1].content_id, Some(7));
    }

    /// The same failed lookup after the store answered another request is that
    /// request's own failure, so the check reports the component and continues.
    #[test]
    fn a_store_that_answered_another_request_degrades_the_failed_lookup() {
        let component = scheme("1.0");
        let mut widgets = HashMap::new();
        widgets.insert(component.directory_name.clone(), 9);
        let registry = RegistryIdCache::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        let result = check(
            std::slice::from_ref(&component),
            &[],
            true,
            &lookup,
            false,
            &dead_client(),
        )
        .unwrap();

        assert_eq!(result.check_failures.len(), 1);
        assert_eq!(
            result.check_failures[0].reason,
            "failed to fetch store entry"
        );
        assert_eq!(result.check_failures[0].content_id, Some(9));
    }

    /// A request the client refused to build is not evidence the store is
    /// unreachable: only a failed connection is.
    #[test]
    fn a_network_failure_that_is_not_a_connection_failure_stays_a_diagnostic() {
        let component = scheme("1.0");
        let mut widgets = HashMap::new();
        widgets.insert(component.directory_name.clone(), 9);
        let registry = RegistryIdCache::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };
        let client =
            ApiClient::new(&Config::new().with_api_url("ftp://127.0.0.1:0/ocs/v1")).unwrap();

        let result = check(
            std::slice::from_ref(&component),
            &[],
            false,
            &lookup,
            false,
            &client,
        )
        .unwrap();

        assert_eq!(result.check_failures.len(), 1);
        assert_eq!(
            result.check_failures[0].reason,
            "failed to fetch store entry"
        );
    }
}
