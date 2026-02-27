// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

/// Returns the user's data directory, respecting XDG_DATA_HOME.
pub(crate) fn data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".local/share"))
}

/// Returns the user's cache directory, respecting XDG_CACHE_HOME.
pub(crate) fn cache_home() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| user_home().join(".cache"))
}

/// Returns the KNewStuff3 registry directory.
pub(crate) fn knewstuff_dir() -> PathBuf {
    data_home().join("knewstuff3")
}

/// Returns true if KDE Plasma desktop environment is detected.
pub(crate) fn is_kde() -> bool {
    std::env::var("KDE_SESSION_VERSION").is_ok()
}

/// Gets the user's home directory, even when running with sudo.
fn user_home() -> PathBuf {
    if let Ok(sudo_home) = std::env::var("SUDO_USER_HOME") {
        return PathBuf::from(sudo_home);
    }

    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        if let Ok(output) = std::process::Command::new("getent")
            .args(["passwd", &sudo_user])
            .output()
            && let Ok(line) = String::from_utf8(output.stdout)
            && let Some(home) = line.split(':').nth(5)
        {
            return PathBuf::from(home);
        }
        return PathBuf::from(format!("/home/{}", sudo_user));
    }

    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default())
}
