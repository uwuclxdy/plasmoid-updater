// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const CONFIG_FILE_NAME: &str = "plasmoid-updater.toml";

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct TomlConfig {
    excluded_packages: Vec<String>,
    update_all_by_default: bool,
    assume_yes: bool,
    prompt_restart: bool,
}

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub inner: libplasmoid_updater::Config,
    pub update_all_by_default: bool,
    pub assume_yes: bool,
}

impl std::ops::Deref for CliConfig {
    type Target = libplasmoid_updater::Config;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl CliConfig {
    /// Loads CLI configuration from TOML config file.
    ///
    /// Uses the library's default embedded widgets-id table unless
    /// a custom path is explicitly provided.
    pub fn load() -> libplasmoid_updater::Result<Self> {
        Self::load_with_widgets_id(None)
    }

    /// Loads CLI configuration with an optional custom widgets-id file.
    ///
    /// If `widgets_id_path` is provided, it overrides the library's
    /// embedded widgets-id table. Otherwise, the library's default
    /// embedded table is used.
    pub fn load_with_widgets_id(
        widgets_id_path: Option<&Path>,
    ) -> libplasmoid_updater::Result<Self> {
        let toml_config = Self::load_toml_config()?;

        let mut inner = libplasmoid_updater::Config::new()
            .with_excluded_packages(toml_config.excluded_packages.clone())
            .with_restart(if toml_config.prompt_restart {
                libplasmoid_updater::RestartBehavior::Prompt
            } else {
                libplasmoid_updater::RestartBehavior::Never
            });

        if let Some(path) = widgets_id_path {
            let widgets_id_table = Self::load_widgets_id_table_from(path)?;
            inner = inner.with_widgets_id_table(widgets_id_table);
        }

        Ok(Self {
            inner,
            update_all_by_default: toml_config.update_all_by_default,
            assume_yes: toml_config.assume_yes,
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
        Ok(libplasmoid_updater::Config::parse_widgets_id(&content))
    }

    pub fn edit_config() -> libplasmoid_updater::Result<()> {
        let path = config_path().ok_or_else(|| {
            libplasmoid_updater::Error::other("could not determine config directory")
        })?;

        ensure_config_exists(&path)?;
        open_in_editor(&path)
    }
}

fn ensure_config_exists(path: &Path) -> libplasmoid_updater::Result<()> {
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

fn create_default_config(path: &Path) -> libplasmoid_updater::Result<()> {
    let default_content = r#"# plasmoid-updater configuration
# excluded_packages = ["widget-name", "another.widget"]
# update_all_by_default = false
# assume_yes = false  # automatically confirm all updates without prompting
# prompt_restart = true
"#;
    fs::write(path, default_content).map_err(|e| {
        libplasmoid_updater::Error::other(format!(
            "failed to create config file {}: {e}",
            path.display()
        ))
    })
}

fn open_in_editor(path: &Path) -> libplasmoid_updater::Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
    std::process::Command::new(&editor)
        .arg(path)
        .status()
        .map_err(|e| {
            libplasmoid_updater::Error::other(format!("failed to open editor {editor}: {e}"))
        })?;
    Ok(())
}
