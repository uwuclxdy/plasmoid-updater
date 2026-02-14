// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

/// Configuration for libplasmoid-updater operations.
///
/// This struct contains all configuration options used by the library.
/// Library consumers (like topgrade or other automation tools) can construct
/// this directly without needing config file parsing.
///
/// # Examples
///
/// ## Basic Configuration
///
/// ```rust
/// use libplasmoid_updater::Config;
///
/// let config = Config::new();
/// ```
///
/// ## With Custom Settings
///
/// ```rust
/// use libplasmoid_updater::Config;
/// use std::collections::HashMap;
///
/// let mut widgets_table = HashMap::new();
/// widgets_table.insert("com.example.widget".to_string(), 123456);
///
/// let config = Config::new()
///     .with_excluded_packages(vec!["problematic-widget".to_string()])
///     .with_widgets_id_table(widgets_table)
///     .with_dry_run(true);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Packages to exclude from updates.
    ///
    /// Can match either directory names (e.g., "org.kde.plasma.systemmonitor")
    /// or display names (e.g., "System Monitor"). Components in this list
    /// will be skipped during update operations.
    pub excluded_packages: Vec<String>,

    /// Widget ID fallback table mapping directory names to KDE Store content IDs.
    ///
    /// This table is used as a fallback when content ID resolution via KNewStuff
    /// registry or exact name matching fails. The library uses a three-tier
    /// resolution strategy:
    ///
    /// 1. KNewStuff registry lookup (most reliable)
    /// 2. Exact name match from KDE Store API
    /// 3. Fallback to this widgets_id_table
    ///
    /// # Format
    ///
    /// - Key: Component directory name (e.g., "org.kde.plasma.systemmonitor")
    /// - Value: KDE Store content ID (numeric)
    ///
    /// The CLI application loads this from a `widgets-id` file, but library
    /// consumers can provide it programmatically or leave it empty.
    pub widgets_id_table: HashMap<String, u64>,

    /// When true, check for updates but do not install them.
    pub dry_run: bool,
}

impl Config {
    /// Creates a new configuration with default values.
    ///
    /// Default values:
    /// - `excluded_packages`: empty
    /// - `widgets_id_table`: empty
    /// - `dry_run`: false
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the widgets ID fallback table.
    ///
    /// This table maps component directory names to KDE Store content IDs
    /// and is used as a fallback when other resolution methods fail.
    ///
    /// # Arguments
    ///
    /// * `table` - HashMap mapping directory names to content IDs
    ///
    /// # Example
    ///
    /// ```rust
    /// use libplasmoid_updater::Config;
    /// use std::collections::HashMap;
    ///
    /// let mut table = HashMap::new();
    /// table.insert("org.kde.plasma.systemmonitor".to_string(), 998890);
    ///
    /// let config = Config::new().with_widgets_id_table(table);
    /// ```
    pub fn with_widgets_id_table(mut self, table: HashMap<String, u64>) -> Self {
        self.widgets_id_table = table;
        self
    }

    /// Sets the list of packages to exclude from updates.
    ///
    /// Components in this list will be skipped during update operations.
    /// The list can contain either directory names or display names.
    ///
    /// # Arguments
    ///
    /// * `packages` - Vector of package names to exclude
    ///
    /// # Example
    ///
    /// ```rust
    /// use libplasmoid_updater::Config;
    ///
    /// let config = Config::new()
    ///     .with_excluded_packages(vec![
    ///         "org.kde.plasma.systemmonitor".to_string(),
    ///         "Problematic Widget".to_string(),
    ///     ]);
    /// ```
    pub fn with_excluded_packages(mut self, packages: Vec<String>) -> Self {
        self.excluded_packages = packages;
        self
    }

    /// Sets whether this is a dry run (check only, don't install).
    ///
    /// # Arguments
    ///
    /// * `dry_run` - If true, updates are detected but not installed
    ///
    /// # Example
    ///
    /// ```rust
    /// use libplasmoid_updater::Config;
    ///
    /// let config = Config::new().with_dry_run(true);
    /// ```
    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}
