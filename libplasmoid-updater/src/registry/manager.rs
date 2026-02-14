// SPDX-License-Identifier: MIT OR Apache-2.0
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::{ComponentType, Result};

use super::{registry_path, utils, xml};

/// Entry from a KNewStuff registry file.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub name: String,
    pub version: String,
    pub content_id: u64,
    pub installed_path: PathBuf,
    pub release_date: String,
}

/// Manages KNewStuff registry files for a specific component type.
///
/// Provides a higher-level API for reading, finding, and updating
/// registry entries within a single `.knsregistry` file.
pub struct RegistryManager {
    file_path: PathBuf,
}

impl RegistryManager {
    /// Creates a `RegistryManager` for the given component type.
    /// Returns `None` if the component type has no associated registry file.
    pub fn for_component_type(component_type: ComponentType) -> Option<Self> {
        registry_path(component_type).map(|file_path| Self { file_path })
    }

    /// Returns the path to the registry file.
    pub fn path(&self) -> &Path {
        &self.file_path
    }

    /// Reads all entries from the registry file.
    pub fn read_entries(&self) -> Result<Vec<RegistryEntry>> {
        if !self.file_path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&self.file_path)?;
        Ok(xml::parse_registry_entries(&content))
    }

    /// Loads entries into a map keyed by directory name.
    pub fn load_entry_map(&self) -> HashMap<String, RegistryEntry> {
        let Ok(entries) = self.read_entries() else {
            return HashMap::new();
        };
        entries
            .into_iter()
            .filter_map(|e| {
                let dir_name = utils::extract_directory_name(&e.installed_path)?;
                Some((dir_name, e))
            })
            .collect()
    }

    /// Finds the content ID for a component by its directory name.
    pub fn find_content_id(&self, directory_name: &str) -> Option<u64> {
        self.load_entry_map()
            .get(directory_name)
            .map(|e| e.content_id)
    }
}
