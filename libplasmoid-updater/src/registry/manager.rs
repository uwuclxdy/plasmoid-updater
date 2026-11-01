// SPDX-License-Identifier: GPL-3.0-or-later
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{collections::HashMap, fs, path::PathBuf};

use crate::{Result, types::ComponentType};

use super::{registry_path, utils, xml};

/// Entry from a KNewStuff registry file.
#[derive(Debug, Clone)]
pub(crate) struct RegistryEntry {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) installed_path: PathBuf,
    pub(crate) release_date: String,
}

/// Manages KNewStuff registry files for a specific component type.
///
/// Provides a higher-level API for reading, finding, and updating
/// registry entries within a single `.knsregistry` file.
pub(crate) struct RegistryManager {
    file_path: PathBuf,
}

impl RegistryManager {
    /// Creates a `RegistryManager` for the given component type.
    /// Returns `None` if the component type has no associated registry file.
    pub(crate) fn for_component_type(component_type: ComponentType) -> Option<Self> {
        registry_path(component_type).map(|file_path| Self { file_path })
    }

    /// Reads all entries from the registry file.
    pub(crate) fn read_entries(&self) -> Result<Vec<RegistryEntry>> {
        if !self.file_path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&self.file_path).map_err(|e| crate::Error::Io {
            path: self.file_path.clone(),
            source: e,
        })?;
        Ok(xml::parse_registry_entries(&content))
    }

    /// Loads entries into a map keyed by directory name.
    pub(crate) fn load_entry_map(&self) -> Result<HashMap<String, RegistryEntry>> {
        let entries = self.read_entries()?;
        Ok(entries
            .into_iter()
            .filter_map(|e| {
                let dir_name = utils::extract_directory_name(&e.installed_path)?;
                Some((dir_name, e))
            })
            .collect())
    }
}
