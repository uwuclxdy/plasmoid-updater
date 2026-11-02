// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

const CONFIG_FILE_NAME: &str = "plasmoid-updater.toml";
const CONFIG_DIR_ENV: &str = "PLASMOID_UPDATER_CONFIG_DIR";
/// Points every store request at another OCS base URL. Undocumented on
/// purpose: it exists for mirrors and test fixtures, not for regular use.
const API_URL_ENV: &str = "PLASMOID_UPDATER_API_URL";

fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join(CONFIG_FILE_NAME));
    }
    dirs::config_dir().map(|d| d.join(CONFIG_FILE_NAME))
}

/// Points the config at [`API_URL_ENV`]'s store base URL when it is set and
/// non-empty. The one place the bin calls the library's URL builder.
fn with_api_url(config: libplasmoid_updater::Config) -> libplasmoid_updater::Config {
    match std::env::var(API_URL_ENV) {
        Ok(url) if !url.is_empty() => config.with_api_url(url),
        _ => config,
    }
}

fn config_error(path: &Path, e: std::io::Error) -> libplasmoid_updater::Error {
    libplasmoid_updater::Error::Io {
        path: path.to_path_buf(),
        source: e,
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct TomlConfig {
    excluded_packages: Vec<String>,
    excluded_patterns: Vec<String>,
    update_all_by_default: bool,
    assume_yes: bool,
    prompt_restart: bool,
}

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub inner: libplasmoid_updater::Config,
    pub update_all_by_default: bool,
    pub assume_yes: bool,
    pub prompt_restart: bool,
}

impl CliConfig {
    /// Loads CLI configuration from the TOML config file.
    pub fn load() -> libplasmoid_updater::Result<Self> {
        let toml_config = Self::load_toml_config()?;

        let inner = with_api_url(
            libplasmoid_updater::Config::new()
                .with_excluded_packages(toml_config.excluded_packages)
                .with_excluded_patterns(toml_config.excluded_patterns),
        );

        Ok(Self {
            inner,
            update_all_by_default: toml_config.update_all_by_default,
            assume_yes: toml_config.assume_yes,
            prompt_restart: toml_config.prompt_restart,
        })
    }

    fn load_toml_config() -> libplasmoid_updater::Result<TomlConfig> {
        let Some(path) = config_path() else {
            return Ok(TomlConfig::default());
        };

        if !path.exists() {
            return Ok(TomlConfig::default());
        }

        let content = fs::read_to_string(&path).map_err(|e| config_error(&path, e))?;

        toml::from_str(&content).map_err(|e| {
            libplasmoid_updater::Error::Config(format!(
                "failed to parse config file {}: {e}",
                path.display()
            ))
        })
    }

    pub fn edit_config() -> libplasmoid_updater::Result<()> {
        let path = config_path().ok_or_else(|| {
            libplasmoid_updater::Error::Config("could not determine config directory".into())
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
        fs::create_dir_all(dir).map_err(|e| config_error(dir, e))?;
    }
    Ok(())
}

fn create_default_config(path: &Path) -> libplasmoid_updater::Result<()> {
    let default_content = r#"# plasmoid-updater configuration
# excluded_packages = ["widget-name", "another.widget"]  # exact name or directory match
# excluded_patterns = ["^org\\.kde\\.", "weather"]  # regex, matched against name and directory
# update_all_by_default = false
# assume_yes = false  # automatically confirm all updates without prompting
# prompt_restart = true
"#;
    fs::write(path, default_content).map_err(|e| config_error(path, e))
}

fn open_in_editor(path: &Path) -> libplasmoid_updater::Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".to_string());
    std::process::Command::new(&editor)
        .arg(path)
        .status()
        .map(|_| ())
        .map_err(|e| editor_spawn_error(&editor, &e))
}

/// A failed editor spawn names the editor. `Error::MissingTool` holds only a
/// `&'static str`, and `Error::Io` a touched path, so a runtime editor name
/// goes into a [`libplasmoid_updater::Error::Config`] message.
fn editor_spawn_error(editor: &str, e: &std::io::Error) -> libplasmoid_updater::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        libplasmoid_updater::Error::Config(format!("editor '{editor}' not found on PATH"))
    } else {
        libplasmoid_updater::Error::Config(format!("failed to run editor '{editor}': {e}"))
    }
}
