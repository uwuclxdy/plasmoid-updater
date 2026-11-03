// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Stop;

const CONFIG_FILE_NAME: &str = "plasmoid-updater.toml";
const CONFIG_DIR_ENV: &str = "PLASMOID_UPDATER_CONFIG_DIR";
/// Points every store request at another OCS base URL. Undocumented on
/// purpose: it exists for mirrors and test fixtures, not for regular use.
const API_URL_ENV: &str = "PLASMOID_UPDATER_API_URL";

fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
}

/// The directory holding the config file, from the environment override or the
/// platform's config dir.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os(CONFIG_DIR_ENV)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
}

/// The config directory when a path component that would have to be a
/// directory is not one. That is the caller's mistake, not a failure of the run.
pub fn unusable_config_dir() -> Option<PathBuf> {
    let dir = config_dir()?;
    match fs::metadata(&dir) {
        Ok(metadata) if !metadata.is_dir() => Some(dir),
        Ok(_) => None,
        // A path below a regular file can never resolve to a directory.
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => Some(dir),
        // A path whose last component exists but does not resolve is a dangling
        // symlink, not a missing directory: the run still cannot use it.
        Err(_) => fs::symlink_metadata(&dir).ok().map(|_| dir),
    }
}

/// Points the config at [`API_URL_ENV`]'s store base URL when it is set and
/// non-empty. The one place the bin calls the library's URL builder.
fn with_api_url(config: libplasmoid_updater::Config) -> libplasmoid_updater::Config {
    match std::env::var(API_URL_ENV) {
        Ok(url) if !url.is_empty() => config.with_api_url(url),
        _ => config,
    }
}

fn parse_error(path: &Path, e: &toml::de::Error) -> Stop {
    Stop::Fatal(libplasmoid_updater::Error::Config(format!(
        "failed to parse config file {path:?}: {e}"
    )))
}

fn config_error(path: &Path, e: std::io::Error) -> Stop {
    Stop::Io {
        path: path.to_path_buf(),
        source: e,
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct TomlConfig {
    excluded_packages: Vec<String>,
    excluded_patterns: Vec<String>,
    update_all_by_default: bool,
    assume_yes: bool,
    prompt_restart: bool,
}

/// A key the 0.1.x releases documented and the run no longer reads. It is no
/// `TomlConfig` field, so the unknown-key error never offers it, and it is
/// dropped before the strict pass, so a config holding it still loads.
const RETIRED_KEY: &str = "verbosity";

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub inner: libplasmoid_updater::Config,
    pub update_all_by_default: bool,
    pub assume_yes: bool,
    pub prompt_restart: bool,
}

impl CliConfig {
    /// Loads CLI configuration from the TOML config file.
    pub fn load() -> Result<Self, Stop> {
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

    fn load_toml_config() -> Result<TomlConfig, Stop> {
        let Some(path) = config_path() else {
            return Ok(TomlConfig::default());
        };

        if !path.exists() {
            return Ok(TomlConfig::default());
        }

        let content = fs::read_to_string(&path).map_err(|e| config_error(&path, e))?;

        // A file without the retired key gets the strict pass on its own text,
        // so toml's error keeps the line/column snippet.
        let mut table: toml::Table =
            toml::from_str(&content).map_err(|e| parse_error(&path, &e))?;
        if table.remove(RETIRED_KEY).is_none() {
            return toml::from_str(&content).map_err(|e| parse_error(&path, &e));
        }
        log::warn!(
            "config key `{RETIRED_KEY}` in {path:?} is no longer used; remove it from the config (-q and -v set the log level)"
        );
        table.try_into().map_err(|e| parse_error(&path, &e))
    }

    pub fn edit_config() -> Result<(), Stop> {
        let path = config_path().ok_or_else(|| {
            Stop::Fatal(libplasmoid_updater::Error::Config(
                "could not determine config directory".into(),
            ))
        })?;

        ensure_config_exists(&path)?;
        open_in_editor(&path)
    }
}

fn ensure_config_exists(path: &Path) -> Result<(), Stop> {
    if path.exists() {
        return Ok(());
    }

    create_config_directory(path)?;
    create_default_config(path)
}

fn create_config_directory(path: &Path) -> Result<(), Stop> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| config_error(dir, e))?;
    }
    Ok(())
}

fn create_default_config(path: &Path) -> Result<(), Stop> {
    let default_content = r#"# plasmoid-updater configuration
# excluded_packages = ["widget-name", "another.widget"]  # exact name or directory match
# excluded_patterns = ["^org\\.kde\\.", "weather"]  # regex, matched against name and directory
# update_all_by_default = false
# assume_yes = false  # automatically confirm all updates without prompting
# prompt_restart = true
"#;
    fs::write(path, default_content).map_err(|e| config_error(path, e))
}

/// Opens the config in `$EDITOR`, the way git does: the variable may hold a
/// command with its own arguments (`code --wait`), so the shell splits it and
/// the config path arrives as the command's positional argument.
fn open_in_editor(path: &Path) -> Result<(), Stop> {
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|editor| !editor.trim().is_empty())
        .unwrap_or_else(|| "nano".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg("sh")
        .arg(path)
        .status()
        .map_err(|e| Stop::from(editor_spawn_error(&editor, &e)))?;

    if status.success() {
        Ok(())
    } else if status.code() == Some(127) {
        // `sh` reports 127 when the command it would run does not exist or is
        // not executable. Only a bare name was looked up on PATH; an absolute
        // path names one file, so claiming PATH would misname the search.
        Err(Stop::from(libplasmoid_updater::Error::Config(
            if editor.contains('/') {
                format!(
                    "editor '{editor}' could not be run (exit 127: not found or not executable)"
                )
            } else {
                format!("editor '{editor}' not found on PATH (exit 127)")
            },
        )))
    } else {
        Err(editor_failure(&editor, &status).into())
    }
}

/// A failed editor run names the exit status the way every other command does.
fn editor_failure(editor: &str, status: &std::process::ExitStatus) -> libplasmoid_updater::Error {
    libplasmoid_updater::Error::Config(crate::command_failure(
        &format!("editor '{editor}'"),
        status,
    ))
}

/// A failed shell spawn names the editor. `Error::MissingTool` holds only a
/// `&'static str`, and `Error::Io` a touched path, so a runtime editor name
/// goes into a [`libplasmoid_updater::Error::Config`] message.
fn editor_spawn_error(editor: &str, e: &std::io::Error) -> libplasmoid_updater::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        libplasmoid_updater::Error::Config(format!("editor '{editor}' not found on PATH"))
    } else {
        libplasmoid_updater::Error::Config(format!("failed to run editor '{editor}': {e}"))
    }
}

#[cfg(test)]
mod tests {
    /// A failed editor run names the exit status, never the doubled
    /// `exited with exit status: 1` shape an `ExitStatus` interpolates.
    #[test]
    fn a_failed_editor_names_the_exit_status() {
        use std::os::unix::process::ExitStatusExt as _;

        let error = super::editor_failure("nano", &std::process::ExitStatus::from_raw(1 << 8));

        assert_eq!(
            error.to_string(),
            "configuration error: editor 'nano' exited with status 1"
        );
    }
}
