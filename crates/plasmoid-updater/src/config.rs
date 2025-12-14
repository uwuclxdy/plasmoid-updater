// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use libplasmoid_updater::{Config, Error, Result, Verbosity};
use serde::Deserialize;

const CONFIG_FILE_NAME: &str = "plasmoid-updater.toml";

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir()
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

fn widgets_id_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("widgets-id"))
        .unwrap_or_else(|| PathBuf::from("widgets-id"))
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct TomlConfig {
    excluded_packages: Vec<String>,
    update_all_by_default: bool,
    verbosity: Option<String>,
    prompt_restart: bool,
}

/// cli configuration wrapper that combines toml file parsing with the library's config.
#[derive(Debug, Clone)]
pub struct CliConfig {
    pub inner: Config,
    pub excluded_packages: Vec<String>,
    pub update_all_by_default: bool,
    pub prompt_restart: bool,
}

impl std::ops::Deref for CliConfig {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl CliConfig {
    pub fn load() -> Result<Self> {
        let toml_config = Self::load_toml_config()?;
        let widgets_id_table = Self::load_widgets_id_table()?;

        let verbosity = match toml_config.verbosity.as_deref() {
            Some("quiet") => Verbosity::Quiet,
            Some("verbose") => Verbosity::Verbose,
            _ => Verbosity::Normal,
        };

        let inner = Config::new()
            .with_widgets_id_table(widgets_id_table)
            .with_excluded_packages(toml_config.excluded_packages.clone())
            .with_verbosity(verbosity);

        Ok(Self {
            inner,
            excluded_packages: toml_config.excluded_packages,
            update_all_by_default: toml_config.update_all_by_default,
            prompt_restart: toml_config.prompt_restart,
        })
    }

    fn load_toml_config() -> Result<TomlConfig> {
        let Some(path) = config_path() else {
            return Ok(TomlConfig::default());
        };

        if !path.exists() {
            return Ok(TomlConfig::default());
        }

        let content = fs::read_to_string(&path).map_err(|e| {
            Error::other(format!(
                "failed to read config file {}: {e}",
                path.display()
            ))
        })?;

        toml::from_str(&content).map_err(|e| {
            Error::other(format!(
                "failed to parse config file {}: {e}",
                path.display()
            ))
        })
    }

    fn load_widgets_id_table() -> Result<HashMap<String, u64>> {
        let path = widgets_id_path();
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let content = fs::read_to_string(&path).map_err(|e| {
            Error::other(format!(
                "failed to read widgets-id file {}: {e}",
                path.display()
            ))
        })?;

        let mut table = HashMap::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let parts: Vec<&str> = line.splitn(2, '=').collect();
            if parts.len() == 2 {
                let name = parts[0].trim().to_string();
                if let Ok(id) = parts[1].trim().parse::<u64>() {
                    table.insert(name, id);
                }
            }
        }

        Ok(table)
    }

    pub fn edit_config() -> Result<()> {
        let Some(path) = config_path() else {
            return Err(Error::other("could not determine config directory"));
        };

        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).map_err(|e| {
                    Error::other(format!(
                        "failed to create config directory {}: {e}",
                        dir.display()
                    ))
                })?;
            }

            let default_content = r#"# plasmoid-updater configuration
# excluded_packages = ["widget-name", "another.widget"]
# update_all_by_default = false
# verbosity = "normal"  # quiet, normal, verbose
# prompt_restart = true
"#;
            fs::write(&path, default_content).map_err(|e| {
                Error::other(format!(
                    "failed to create config file {}: {e}",
                    path.display()
                ))
            })?;
        }

        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
        std::process::Command::new(&editor)
            .arg(&path)
            .status()
            .map_err(|e| Error::other(format!("failed to open editor {editor}: {e}")))?;

        Ok(())
    }
}
