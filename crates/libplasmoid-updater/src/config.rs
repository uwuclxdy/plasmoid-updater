// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

/// verbosity level for operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verbosity {
    Quiet,
    #[default]
    Normal,
    Verbose,
}

impl std::fmt::Display for Verbosity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quiet => write!(f, "quiet"),
            Self::Normal => write!(f, "normal"),
            Self::Verbose => write!(f, "verbose"),
        }
    }
}

/// configuration for plasmoid-updater operations.
///
/// this struct contains all configuration options used by the library.
/// consumers (like topgrade) can construct this directly without needing
/// config file parsing.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// packages to exclude from updates (directory names or display names).
    pub excluded_packages: Vec<String>,

    /// when running update without arguments, update all instead of prompting.
    pub update_all_by_default: bool,

    /// verbosity level for output.
    pub verbosity: Verbosity,

    /// whether to prompt for plasmashell restart after updates.
    pub prompt_restart: bool,

    /// widget id fallback table mapping directory names to kde store content ids.
    pub widgets_id_table: HashMap<String, u64>,
}

impl Config {
    /// creates a new config with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// creates a config with the given widgets id table.
    pub fn with_widgets_id_table(mut self, table: HashMap<String, u64>) -> Self {
        self.widgets_id_table = table;
        self
    }

    /// creates a config with the given excluded packages.
    pub fn with_excluded_packages(mut self, packages: Vec<String>) -> Self {
        self.excluded_packages = packages;
        self
    }

    /// creates a config with the given verbosity level.
    pub fn with_verbosity(mut self, verbosity: Verbosity) -> Self {
        self.verbosity = verbosity;
        self
    }
}
