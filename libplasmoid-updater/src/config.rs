// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

/// Default embedded widgets-id mapping file provided by Apdatifier.
///
/// This file maps component directory names to KDE Store content IDs
/// and is used as a fallback when other resolution methods fail.
const DEFAULT_WIDGETS_ID: &str = include_str!("../widgets-id");

/// Controls plasmashell restart behavior after updates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RestartBehavior {
    /// Never restart plasmashell (default).
    #[default]
    Never,
    /// Always restart plasmashell after successful updates that require it.
    Always,
    /// Prompt the user interactively. Falls back to [`Never`](Self::Never) if
    /// stdin is not a terminal.
    Prompt,
}

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
/// use libplasmoid_updater::{Config, RestartBehavior};
/// use std::collections::HashMap;
///
/// let mut widgets_table = HashMap::new();
/// widgets_table.insert("com.example.widget".to_string(), 123456);
///
/// let config = Config::new()
///     .with_excluded_packages(vec!["problematic-widget".to_string()])
///     .with_widgets_id_table(widgets_table)
///     .with_restart(RestartBehavior::Always);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// If `true`, operate on system-wide components (in `/usr/share`).
    /// If `false` (default), operate on user components (in `~/.local/share`).
    /// System operations require root privileges.
    pub system: bool,

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

    /// Controls plasmashell restart behavior after successful updates.
    pub restart: RestartBehavior,

    pub yes: bool,

    /// Maximum number of parallel installation threads.
    ///
    /// `None` (default) uses the number of logical CPU threads available.
    /// `Some(n)` pins the pool to exactly `n` threads.
    pub threads: Option<usize>,
}

impl Config {
    /// Creates a new configuration with default values.
    ///
    /// Default values:
    /// - `system`: false (user components)
    /// - `excluded_packages`: empty
    /// - `widgets_id_table`: loaded from embedded widgets-id file
    /// - `restart`: [`RestartBehavior::Never`]
    ///
    /// The embedded widgets-id table provides fallback content ID mappings
    /// for components that cannot be resolved via KNewStuff registry or
    /// exact name matching.
    pub fn new() -> Self {
        Self {
            widgets_id_table: Self::parse_widgets_id(DEFAULT_WIDGETS_ID),
            ..Default::default()
        }
    }

    /// Sets whether to operate on system-wide components.
    ///
    /// When true, the library scans and updates components in `/usr/share`
    /// instead of `~/.local/share`. System operations require root privileges.
    ///
    /// # Example
    ///
    /// ```rust
    /// use libplasmoid_updater::Config;
    ///
    /// let config = Config::new().with_system(true);
    /// ```
    pub fn with_system(mut self, system: bool) -> Self {
        self.system = system;
        self
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

    /// Sets the list of Plasmoids to exclude from updates.
    ///
    /// Components in this list will be skipped during updates.
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

    /// Sets the plasmashell restart behavior after updates.
    ///
    /// # Example
    ///
    /// ```rust
    /// use libplasmoid_updater::{Config, RestartBehavior};
    ///
    /// let config = Config::new().with_restart(RestartBehavior::Always);
    /// ```
    pub fn with_restart(mut self, restart: RestartBehavior) -> Self {
        self.restart = restart;
        self
    }

    /// Parses a widgets-id table from a string.
    ///
    /// The format is one entry per line: `content_id directory_name`
    /// Lines starting with `#` are comments.
    pub fn parse_widgets_id(content: &str) -> HashMap<String, u64> {
        let mut table = HashMap::new();
        for line in content.lines() {
            if let Some((id, name)) = parse_widgets_id_line(line) {
                table.insert(name, id);
            }
        }
        table
    }

    pub fn with_yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = Some(threads);
        self
    }
}

pub(crate) fn parse_widgets_id_line(line: &str) -> Option<(u64, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    if parts.len() == 2
        && let Ok(id) = parts[0].trim().parse::<u64>()
    {
        return Some((id, parts[1].trim().to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_widgets_id_line_valid() {
        let line = "998890 com.bxabi.bumblebee-indicator";
        let result = parse_widgets_id_line(line);
        assert_eq!(
            result,
            Some((998890, "com.bxabi.bumblebee-indicator".to_string()))
        );
    }

    #[test]
    fn test_parse_widgets_id_line_comment() {
        let line = "#2182964 adhe.menu.11 #Ignored, not a unique ID";
        let result = parse_widgets_id_line(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_widgets_id_line_empty() {
        let line = "";
        let result = parse_widgets_id_line(line);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_widgets_id_table() {
        let content = "998890 com.bxabi.bumblebee-indicator\n\
                       998913 org.kde.plasma.awesomewidget\n\
                       # Comment line\n\
                       1155946 com.dschopf.plasma.qalculate\n";
        let table = Config::parse_widgets_id(content);
        assert_eq!(table.len(), 3);
        assert_eq!(table.get("com.bxabi.bumblebee-indicator"), Some(&998890));
        assert_eq!(table.get("org.kde.plasma.awesomewidget"), Some(&998913));
        assert_eq!(table.get("com.dschopf.plasma.qalculate"), Some(&1155946));
    }

    #[test]
    fn test_default_widgets_id_table_loads() {
        let config = Config::new();
        // Verify the embedded file is loaded and contains expected entries
        assert!(
            !config.widgets_id_table.is_empty(),
            "Default widgets_id_table should not be empty"
        );
        // Check for a few known entries from the widgets-id file
        assert_eq!(
            config.widgets_id_table.get("com.bxabi.bumblebee-indicator"),
            Some(&998890)
        );
        assert_eq!(
            config.widgets_id_table.get("org.kde.plasma.awesomewidget"),
            Some(&998913)
        );
    }

    #[test]
    fn test_config_with_custom_widgets_id_table() {
        let mut custom_table = HashMap::new();
        custom_table.insert("custom.widget".to_string(), 123456);

        let config = Config::new().with_widgets_id_table(custom_table.clone());

        // Should use custom table, not default
        assert_eq!(config.widgets_id_table, custom_table);
        assert_eq!(config.widgets_id_table.get("custom.widget"), Some(&123456));
        // Default entry should not be present
        assert_eq!(
            config.widgets_id_table.get("com.bxabi.bumblebee-indicator"),
            None
        );
    }
}
