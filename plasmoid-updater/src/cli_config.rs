// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::output::Verbosity;

const CONFIG_FILE_NAME: &str = "plasmoid-updater.toml";

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir()
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct TomlConfig {
    excluded_packages: Vec<String>,
    update_all_by_default: bool,
    assume_yes: bool,
    verbosity: Option<String>,
    prompt_restart: bool,
}

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub inner: libplasmoid_updater::Config,
    pub excluded_packages: Vec<String>,
    pub update_all_by_default: bool,
    pub assume_yes: bool,
    pub prompt_restart: bool,
    pub verbosity: Verbosity,
}

impl std::ops::Deref for CliConfig {
    type Target = libplasmoid_updater::Config;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl CliConfig {
    pub fn load_with_widgets_id(
        widgets_id_path: Option<&Path>,
    ) -> libplasmoid_updater::Result<Self> {
        let toml_config = Self::load_toml_config()?;
        let widgets_id_table = if let Some(path) = widgets_id_path {
            Self::load_widgets_id_table_from(path)?
        } else {
            HashMap::new()
        };
        let verbosity = parse_verbosity(&toml_config.verbosity);

        let inner = libplasmoid_updater::Config::new()
            .with_widgets_id_table(widgets_id_table)
            .with_excluded_packages(toml_config.excluded_packages.clone());

        Ok(Self {
            inner,
            excluded_packages: toml_config.excluded_packages,
            update_all_by_default: toml_config.update_all_by_default,
            assume_yes: toml_config.assume_yes,
            prompt_restart: toml_config.prompt_restart,
            verbosity,
        })
    }

    fn load_toml_config() -> libplasmoid_updater::Result<TomlConfig> {
        let Some(path) = config_path() else {
            return Ok(TomlConfig::default());
        };

        if !path.exists() {
            return Ok(TomlConfig::default());
        }

        let content = fs::read_to_string(&path).map_err(|e| {
            libplasmoid_updater::Error::other(format!(
                "failed to read config file {}: {e}",
                path.display()
            ))
        })?;

        toml::from_str(&content).map_err(|e| {
            libplasmoid_updater::Error::other(format!(
                "failed to parse config file {}: {e}",
                path.display()
            ))
        })
    }

    fn load_widgets_id_table_from(
        path: &Path,
    ) -> libplasmoid_updater::Result<HashMap<String, u64>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let content = fs::read_to_string(path).map_err(|e| {
            libplasmoid_updater::Error::other(format!(
                "failed to read file {}: {e}",
                path.display()
            ))
        })?;
        Ok(parse_widgets_id_table(&content))
    }

    pub fn edit_config() -> libplasmoid_updater::Result<()> {
        let path = config_path()
            .ok_or_else(|| libplasmoid_updater::Error::other("could not determine config directory"))?;

        ensure_config_exists(&path)?;
        open_in_editor(&path)
    }
}

fn parse_verbosity(verbosity: &Option<String>) -> Verbosity {
    match verbosity.as_deref() {
        Some("quiet") => Verbosity::Quiet,
        Some("verbose") => Verbosity::Verbose,
        _ => Verbosity::Normal,
    }
}

fn parse_widgets_id_table(content: &str) -> HashMap<String, u64> {
    let mut table = HashMap::new();
    for line in content.lines() {
        if let Some((id, name)) = parse_widgets_id_line(line) {
            table.insert(name, id);
        }
    }
    table
}

fn parse_widgets_id_line(line: &str) -> Option<(u64, String)> {
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

fn ensure_config_exists(path: &PathBuf) -> libplasmoid_updater::Result<()> {
    if path.exists() {
        return Ok(());
    }

    create_config_directory(path)?;
    create_default_config(path)
}

fn create_config_directory(path: &Path) -> libplasmoid_updater::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| {
            libplasmoid_updater::Error::other(format!(
                "failed to create config directory {}: {e}",
                dir.display()
            ))
        })?;
    }
    Ok(())
}

fn create_default_config(path: &PathBuf) -> libplasmoid_updater::Result<()> {
    let default_content = r#"# plasmoid-updater configuration
# excluded_packages = ["widget-name", "another.widget"]
# update_all_by_default = false
# assume_yes = false  # automatically confirm all updates without prompting
# verbosity = "normal"  # quiet, normal, verbose
# prompt_restart = true
"#;
    fs::write(path, default_content).map_err(|e| {
        libplasmoid_updater::Error::other(format!(
            "failed to create config file {}: {e}",
            path.display()
        ))
    })
}

fn open_in_editor(path: &PathBuf) -> libplasmoid_updater::Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
    std::process::Command::new(&editor)
        .arg(path)
        .status()
        .map_err(|e| {
            libplasmoid_updater::Error::other(format!("failed to open editor {editor}: {e}"))
        })?;
    Ok(())
}
