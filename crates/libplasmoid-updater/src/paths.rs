// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

pub(crate) fn data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".local/share"))
}

pub(crate) fn cache_home() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".cache"))
}

/// gets the actual user's home directory, even when running with sudo
fn user_home() -> PathBuf {
    // when running with sudo, prefer SUDO_USER_HOME (set by our escalation code)
    if let Ok(sudo_home) = std::env::var("SUDO_USER_HOME") {
        return PathBuf::from(sudo_home);
    }

    // fallback to HOME, then dirs::home_dir()
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default())
}

pub(crate) fn knewstuff_dir() -> PathBuf {
    data_home().join("knewstuff3")
}
