// SPDX-License-Identifier: GPL-3.0-or-later
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::path::PathBuf;

use crate::{Result, types::ComponentType};

use super::{read_registry_file, registry_path};

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
    ///
    /// A missing registry file reads as empty; a registry file that exists but
    /// cannot be inspected (a permission error, a parent that is not a
    /// directory) is an error, never an empty registry.
    pub(crate) fn read_entries(&self) -> Result<Vec<RegistryEntry>> {
        let exists = self.file_path.try_exists().map_err(|e| crate::Error::Io {
            path: self.file_path.clone(),
            source: e,
        })?;
        if !exists {
            return Ok(Vec::new());
        }
        read_registry_file(&self.file_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parse_error_names_the_registry_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colorschemes.knsregistry");
        std::fs::write(&path, "<hotnewstuffregistry><stuff category=\"112\">").unwrap();
        let manager = RegistryManager {
            file_path: path.clone(),
        };

        let err = manager.read_entries().unwrap_err();

        assert!(matches!(&err, crate::Error::Parse { .. }), "{err}");
        let prefix = format!("failed to parse knewstuff registry: {}: ", path.display());
        assert!(err.to_string().starts_with(&prefix), "{err}");
    }

    #[test]
    fn a_registry_path_that_cannot_be_inspected_is_an_error_not_an_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, b"").unwrap();
        let path = file.join("colorschemes.knsregistry");
        let manager = RegistryManager {
            file_path: path.clone(),
        };

        let err = manager.read_entries().unwrap_err();

        assert!(
            matches!(&err, crate::Error::Io { path: reported, .. } if *reported == path),
            "{err}"
        );
    }
}
