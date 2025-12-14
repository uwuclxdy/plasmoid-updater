// SPDX-License-Identifier: MIT OR Apache-2.0
//
// This implementation is based on:
// - Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// - KDE Discover's KNewStuff backend (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+
//
// The update detection algorithm, KDE Store API interaction, and widget ID resolution
// approach are derived from Apdatifier's shell scripts. The KNewStuff registry format
// and installation process knowledge comes from KDE Discover's source code.

pub mod api;
pub mod backup;
pub mod config;
pub mod discovery;
pub mod error;
pub mod installer;
pub mod registry;
pub mod resolver;
pub mod types;
pub mod version;

use std::sync::Arc;

use parking_lot::Mutex;
use rayon::prelude::*;

pub use api::ApiClient;
pub use backup::{backup_component, restore_from_backup};
pub use config::{Config, Verbosity};
pub use discovery::scan_installed_components;
pub use error::{Error, Result};
pub use installer::{
    any_requires_restart, create_download_client, restart_plasmashell, update_component,
    update_component_with_client, update_components, update_components_with_client,
};
pub use registry::{scan_registry_components, update_registry_after_install};
pub use resolver::{find_store_entry, resolve_content_id, select_download_url};
pub use types::{
    AvailableUpdate, CheckResult, ComponentType, DownloadLink, InstalledComponent, JsonOutput,
    KPluginInfo, PackageMetadata, StoreEntry, UpdateSummary,
};
pub use version::{
    compare as compare_versions, is_update_available, normalize as normalize_version,
};

/// checks for available updates for user-installed components.
pub fn check_updates(config: &Config) -> Result<Vec<AvailableUpdate>> {
    Ok(check_updates_detailed(config, false)?.updates)
}

/// checks for available updates for system-wide components.
pub fn check_updates_system(config: &Config) -> Result<Vec<AvailableUpdate>> {
    Ok(check_updates_detailed(config, true)?.updates)
}

/// checks for updates with detailed results including unresolved components.
pub fn check_updates_with_details(config: &Config) -> Result<CheckResult> {
    check_updates_detailed(config, false)
}

/// checks for updates on system components with detailed results.
pub fn check_updates_system_with_details(config: &Config) -> Result<CheckResult> {
    check_updates_detailed(config, true)
}

fn check_updates_detailed(config: &Config, system: bool) -> Result<CheckResult> {
    let components = scan_installed_components(system)?;

    if components.is_empty() {
        return Ok(CheckResult::new());
    }

    let (registry_components, regular_components): (Vec<_>, Vec<_>) = components
        .into_iter()
        .partition(|c| c.component_type.registry_only());

    let client = ApiClient::new();

    let regular_types: Vec<ComponentType> = if system {
        ComponentType::all()
            .iter()
            .filter(|t| !t.registry_only())
            .copied()
            .collect()
    } else {
        ComponentType::all_user()
            .iter()
            .filter(|t| !t.registry_only())
            .copied()
            .collect()
    };

    let store_entries = if !regular_types.is_empty() && !regular_components.is_empty() {
        client.fetch_all_content(&regular_types)?
    } else {
        Vec::new()
    };

    let widgets_id_table = &config.widgets_id_table;

    // process regular components in parallel
    let result = Arc::new(Mutex::new(CheckResult::new()));

    regular_components.par_iter().for_each(|component| {
        let update_result =
            process_component_check(component, &store_entries, widgets_id_table, None);

        let mut result = result.lock();
        match update_result {
            ComponentCheckResult::Update(update) => result.add_update(*update),
            ComponentCheckResult::Unresolved(name, reason) => result.add_unresolved(name, reason),
            ComponentCheckResult::CheckFailed(name, reason) => {
                result.add_check_failure(name, reason)
            }
            ComponentCheckResult::NoUpdate => {}
        }
    });

    // process registry-only components in parallel (with individual API fetches)
    let registry_ids: Vec<_> = registry_components
        .iter()
        .filter_map(|c| resolve_content_id(c, &store_entries, widgets_id_table).map(|id| (c, id)))
        .collect();

    let content_ids: Vec<u64> = registry_ids.iter().map(|(_, id)| *id).collect();
    let fetched_entries = client.fetch_content_details_batch(&content_ids);

    for ((component, content_id), fetch_result) in registry_ids.iter().zip(fetched_entries) {
        match fetch_result {
            Ok(entry) => {
                let mut result = result.lock();
                if !version::is_update_available_with_date(
                    &component.version,
                    &entry.version,
                    &component.release_date,
                    &entry.changed_date,
                ) {
                    continue;
                }

                let Some(download_info) = select_download_with_info(&entry, &entry.version) else {
                    result.add_check_failure(
                        component.name.clone(),
                        "no download url available".to_string(),
                    );
                    continue;
                };

                let mut update = AvailableUpdate::new(
                    (*component).clone(),
                    *content_id,
                    entry.version.clone(),
                    download_info.url,
                    entry.changed_date.clone(),
                );
                update.checksum = download_info.checksum;
                update.download_size = download_info.size_kb.map(|kb| kb * 1024);
                result.add_update(update);
            }
            Err(e) => {
                result.lock().add_check_failure(
                    component.name.clone(),
                    format!("failed to fetch store entry: {e}"),
                );
            }
        }
    }

    // add unresolved registry components
    for component in &registry_components {
        if !registry_ids
            .iter()
            .any(|(c, _)| c.directory_name == component.directory_name)
        {
            result.lock().add_unresolved(
                component.name.clone(),
                "could not match to kde store entry".to_string(),
            );
        }
    }

    Ok(Arc::try_unwrap(result).expect("arc unwrap").into_inner())
}

enum ComponentCheckResult {
    Update(Box<AvailableUpdate>),
    Unresolved(String, String),
    CheckFailed(String, String),
    NoUpdate,
}

fn process_component_check(
    component: &InstalledComponent,
    store_entries: &[StoreEntry],
    widgets_id_table: &std::collections::HashMap<String, u64>,
    _client: Option<&ApiClient>,
) -> ComponentCheckResult {
    let content_id = match resolve_content_id(component, store_entries, widgets_id_table) {
        Some(id) => id,
        None => {
            log::debug!(
                "**resolver:** could not resolve id for '{}'",
                component.name
            );
            return ComponentCheckResult::Unresolved(
                component.name.clone(),
                "could not match to kde store entry".to_string(),
            );
        }
    };

    let entry = match find_store_entry(store_entries, content_id) {
        Some(e) => e,
        None => {
            log::debug!(
                "**resolver:** store entry not found for id {} ({})",
                content_id,
                component.name
            );
            return ComponentCheckResult::Unresolved(
                component.name.clone(),
                format!("store entry {content_id} not in fetched data"),
            );
        }
    };

    if !version::is_update_available_with_date(
        &component.version,
        &entry.version,
        &component.release_date,
        &entry.changed_date,
    ) {
        return ComponentCheckResult::NoUpdate;
    }

    let Some(download_info) = select_download_with_info(entry, &entry.version) else {
        log::warn!(
            "**resolver:** no download url for '{}' (id: {})",
            component.name,
            content_id
        );
        return ComponentCheckResult::CheckFailed(
            component.name.clone(),
            "no download url available".to_string(),
        );
    };

    let mut update = AvailableUpdate::new(
        component.clone(),
        content_id,
        entry.version.clone(),
        download_info.url,
        entry.changed_date.clone(),
    );
    update.checksum = download_info.checksum;
    update.download_size = download_info.size_kb.map(|kb| kb * 1024);

    ComponentCheckResult::Update(Box::new(update))
}

struct DownloadInfo {
    url: String,
    checksum: Option<String>,
    size_kb: Option<u64>,
}

fn select_download_with_info(entry: &StoreEntry, target_version: &str) -> Option<DownloadInfo> {
    if entry.download_links.is_empty() {
        return None;
    }

    let link = if entry.download_links.len() == 1 {
        &entry.download_links[0]
    } else {
        entry
            .download_links
            .iter()
            .find(|l| l.version == target_version)
            .or_else(|| entry.download_links.first())?
    };

    Some(DownloadInfo {
        url: link.url.clone(),
        checksum: link.checksum.clone(),
        size_kb: link.size_kb,
    })
}

/// updates all user-installed components that have available updates.
pub fn update_all(config: &Config) -> Result<UpdateSummary> {
    update_all_impl(config, false)
}

/// updates all system-wide components that have available updates.
pub fn update_all_system(config: &Config) -> Result<UpdateSummary> {
    update_all_impl(config, true)
}

fn update_all_impl(config: &Config, system: bool) -> Result<UpdateSummary> {
    let updates = if system {
        check_updates_system(config)?
    } else {
        check_updates(config)?
    };

    let summary = update_components(&updates, &config.excluded_packages);

    Ok(summary)
}

/// returns all installed components.
pub fn list_installed(system: bool) -> Result<Vec<InstalledComponent>> {
    scan_installed_components(system)
}
