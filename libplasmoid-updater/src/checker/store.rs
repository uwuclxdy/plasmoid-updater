// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;

use crate::{
    Result,
    api::ApiClient,
    types::{ComponentType, InstalledComponent, StoreEntry},
};

use super::IdLookup;

/// The store entries a check could read, and whether the store answered any
/// request at all.
pub(crate) struct StoreFetch {
    pub(crate) entries: Vec<StoreEntry>,
    pub(crate) answered: bool,
}

pub(crate) fn partition_components(
    components: Vec<InstalledComponent>,
) -> (Vec<InstalledComponent>, Vec<InstalledComponent>) {
    components
        .into_iter()
        .partition(|c| c.component_type.registry_only())
}

/// Fetches the minimum set of store entries needed to evaluate `regular_components`,
/// ensuring each entry is retrieved at most once.
///
/// Strategy:
/// 1. Resolve content IDs from local data (registry cache + widgets-id table) — no network.
/// 2. Fetch catalog pages for every distinct component type present, regardless of
///    whether IDs are already known. A single catalog page covers ≤100 entries,
///    converting O(n) targeted fetches into O(distinct_types) catalog requests.
/// 3. For known IDs genuinely absent from the catalog, issue one targeted request per ID.
pub(crate) fn fetch_store_entries(
    client: &ApiClient,
    regular_components: &[InstalledComponent],
    lookup: &IdLookup,
) -> Result<StoreFetch> {
    if regular_components.is_empty() {
        return Ok(StoreFetch {
            entries: Vec::new(),
            answered: false,
        });
    }

    let known_ids: HashSet<u64> = regular_components
        .iter()
        .filter_map(|c| resolve_id_locally(c, lookup))
        .collect();

    // Always fetch catalog for all distinct component types — not just unresolved ones.
    // When all IDs are locally known, skipping this forces one targeted request per ID.
    let types = distinct_types(regular_components);
    let catalog = client.fetch_all(&types);
    let mut answered = catalog.answered;

    // Targeted fetch only for known IDs genuinely absent from the catalog
    // (e.g. old/unlisted components that no longer appear in recent pages).
    let listed_ids: HashSet<u64> = catalog.entries.iter().map(|e| e.id).collect();
    let missing_ids: Vec<u64> = known_ids
        .iter()
        .copied()
        .filter(|id| !listed_ids.contains(id))
        .collect();

    let mut entries = catalog.entries;
    for (id, detail) in missing_ids.iter().zip(client.fetch_details(&missing_ids)) {
        answered |= detail.answered;
        match detail.result {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                log::warn!(target: "api", "store entry {id} could not be fetched: {error}");
            }
        }
    }

    if !answered && let Some(error) = catalog.error {
        return Err(error);
    }

    Ok(StoreFetch { entries, answered })
}

fn resolve_id_locally(component: &InstalledComponent, lookup: &IdLookup) -> Option<u64> {
    lookup.registry_id(component).or_else(|| {
        lookup
            .widgets_id_table
            .get(&component.directory_name)
            .copied()
    })
}

fn distinct_types(components: &[InstalledComponent]) -> Vec<ComponentType> {
    let mut seen = HashSet::new();
    components
        .iter()
        .map(|c| c.component_type)
        .filter(|t| seen.insert(*t))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::api::ApiClient;
    use crate::config::Config;
    use crate::registry::RegistryInstall;
    use crate::testutil::{not_found, ok, serve};

    fn component(
        name: &str,
        directory_name: &str,
        component_type: ComponentType,
        version: &str,
    ) -> InstalledComponent {
        InstalledComponent {
            name: name.to_string(),
            directory_name: directory_name.to_string(),
            version: version.to_string(),
            component_type,
            path: PathBuf::from("/usr/share/plasma/plasmoids").join(directory_name),
            is_system: false,
            release_date: String::new(),
        }
    }

    fn client(base: &str) -> ApiClient {
        ApiClient::new(&Config::new().with_api_url(base)).unwrap()
    }

    /// A client pointed at port 0, which no process can own, so no store can
    /// answer it.
    fn dead_client() -> ApiClient {
        client("http://127.0.0.1:0/ocs/v1")
    }

    fn entry_xml(id: u64) -> String {
        format!(
            "<content><id>{id}</id><name>Item {id}</name><version>2.0</version>\
             <typeid>705</typeid><changed>2026-10-01</changed></content>"
        )
    }

    fn ocs(total: u32, content: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>100</statuscode>\
             <message></message><totalitems>{total}</totalitems><itemsperpage>100</itemsperpage>\
             </meta><data>{content}</data></ocs>"
        )
    }

    fn is_catalog(target: &str) -> bool {
        target.starts_with("/ocs/v1/content/data?")
    }

    #[test]
    fn partition_components_splits_registry_only_types() {
        let (registry_only, regular) = partition_components(vec![
            component("Scheme", "Scheme.colors", ComponentType::ColorScheme, "1.0"),
            component(
                "Widget",
                "org.test.widget",
                ComponentType::PlasmaWidget,
                "1.0",
            ),
            component("Icons", "MyIcons", ComponentType::IconTheme, "1.0"),
        ]);

        let registry_names: Vec<&str> = registry_only
            .iter()
            .map(|item| item.name.as_str())
            .collect();
        let regular_names: Vec<&str> = regular.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(registry_names, vec!["Scheme", "Icons"]);
        assert_eq!(regular_names, vec!["Widget"]);
    }

    #[test]
    fn no_regular_components_makes_no_request() {
        let widgets = HashMap::new();
        let registry = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        let fetch = fetch_store_entries(&dead_client(), &[], &lookup).unwrap();

        assert!(fetch.entries.is_empty());
        assert!(
            !fetch.answered,
            "an empty check must not claim the store answered"
        );
    }

    #[test]
    fn a_catalog_entry_satisfies_a_known_id_without_a_per_id_fetch() {
        let (base, requests) = serve(|target| {
            if is_catalog(target) {
                ok(ocs(1, &entry_xml(7)))
            } else {
                not_found()
            }
        });
        let mut widgets = HashMap::new();
        widgets.insert("org.test.widget".to_string(), 7);
        let registry = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };
        let component = component(
            "Widget",
            "org.test.widget",
            ComponentType::PlasmaWidget,
            "1.0",
        );

        let fetch =
            fetch_store_entries(&client(&base), std::slice::from_ref(&component), &lookup).unwrap();

        let ids: Vec<u64> = fetch.entries.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec![7]);
        assert!(fetch.answered);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "a catalog entry must satisfy the known id without a per-id request"
        );
    }

    #[test]
    fn a_known_id_missing_from_the_catalog_is_fetched_by_id() {
        let (base, _requests) = serve(|target| {
            if is_catalog(target) {
                ok(ocs(0, ""))
            } else {
                ok(ocs(1, &entry_xml(9)))
            }
        });
        let mut widgets = HashMap::new();
        widgets.insert("org.test.widget".to_string(), 9);
        let registry = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };
        let component = component(
            "Widget",
            "org.test.widget",
            ComponentType::PlasmaWidget,
            "1.0",
        );

        let fetch =
            fetch_store_entries(&client(&base), std::slice::from_ref(&component), &lookup).unwrap();

        let ids: Vec<u64> = fetch.entries.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec![9]);
        assert!(fetch.answered);
    }

    #[test]
    fn two_components_with_one_known_id_fetch_it_once() {
        let (base, requests) = serve(|target| {
            if is_catalog(target) {
                ok(ocs(0, ""))
            } else {
                ok(ocs(1, &entry_xml(9)))
            }
        });
        let mut widgets = HashMap::new();
        widgets.insert("first.widget".to_string(), 9);
        widgets.insert("second.widget".to_string(), 9);
        let registry = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };
        let components = [
            component("First", "first.widget", ComponentType::PlasmaWidget, "1.0"),
            component(
                "Second",
                "second.widget",
                ComponentType::PlasmaWidget,
                "1.0",
            ),
        ];

        let fetch = fetch_store_entries(&client(&base), &components, &lookup).unwrap();

        let ids: Vec<u64> = fetch.entries.iter().map(|entry| entry.id).collect();
        assert_eq!(
            ids,
            vec![9],
            "one id must be fetched once, not once per component"
        );
        assert_eq!(
            requests.load(Ordering::SeqCst),
            2,
            "one catalog request and one per-id request"
        );
    }

    #[test]
    fn a_store_that_never_answered_returns_the_catalog_error() {
        let widgets = HashMap::new();
        let registry = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };
        let component = component(
            "Widget",
            "org.test.widget",
            ComponentType::PlasmaWidget,
            "1.0",
        );

        let error = fetch_store_entries(&dead_client(), std::slice::from_ref(&component), &lookup)
            .err()
            .expect("an unreachable store must fail the fetch");

        assert!(matches!(error, crate::Error::Network(_)), "{error:?}");
    }

    #[test]
    fn a_received_catalog_response_is_an_answer_even_when_its_payload_is_garbage() {
        let (base, _requests) = serve(|_| ok("<html>maintenance</html>"));
        let widgets = HashMap::new();
        let registry = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };
        let component = component(
            "Widget",
            "org.test.widget",
            ComponentType::PlasmaWidget,
            "1.0",
        );

        let fetch =
            fetch_store_entries(&client(&base), std::slice::from_ref(&component), &lookup).unwrap();

        assert!(fetch.entries.is_empty());
        assert!(
            fetch.answered,
            "a received response is an answer even when its payload does not parse"
        );
    }

    #[test]
    fn a_failed_per_id_lookup_keeps_the_catalog_entries() {
        let (base, _requests) = serve(|target| {
            if is_catalog(target) {
                ok(ocs(1, &entry_xml(7)))
            } else {
                not_found()
            }
        });
        let mut widgets = HashMap::new();
        widgets.insert("org.test.widget".to_string(), 9);
        let registry = HashMap::new();
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };
        let component = component(
            "Widget",
            "org.test.widget",
            ComponentType::PlasmaWidget,
            "1.0",
        );

        let fetch =
            fetch_store_entries(&client(&base), std::slice::from_ref(&component), &lookup).unwrap();

        let ids: Vec<u64> = fetch.entries.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, vec![7], "a failed per-id lookup dropped the catalog");
        assert!(fetch.answered);
    }

    #[test]
    fn a_local_id_comes_from_the_registry_before_the_widgets_table() {
        let component = component(
            "Widget",
            "org.test.widget",
            ComponentType::PlasmaWidget,
            "1.0",
        );
        let mut widgets = HashMap::new();
        widgets.insert("org.test.widget".to_string(), 77);
        let mut registry = HashMap::new();
        registry.insert(
            RegistryInstall {
                component_type: ComponentType::PlasmaWidget,
                path: crate::registry::normalize_path(&component.path),
            },
            42,
        );
        let lookup = IdLookup {
            widgets_id_table: &widgets,
            registry_id_cache: &registry,
        };

        assert_eq!(resolve_id_locally(&component, &lookup), Some(42));
    }

    #[test]
    fn distinct_types_keeps_one_of_each() {
        let components = [
            component("A", "a.widget", ComponentType::PlasmaWidget, "1.0"),
            component("B", "a.colors", ComponentType::ColorScheme, "1.0"),
            component("C", "c.widget", ComponentType::PlasmaWidget, "1.0"),
            component("D", "d.colors", ComponentType::ColorScheme, "1.0"),
            component("E", "e.kwin", ComponentType::KWinEffect, "1.0"),
        ];

        assert_eq!(
            distinct_types(&components),
            vec![
                ComponentType::PlasmaWidget,
                ComponentType::ColorScheme,
                ComponentType::KWinEffect
            ]
        );
    }
}
