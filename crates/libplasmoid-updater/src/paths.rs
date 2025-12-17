// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;

pub(crate) fn data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"))
}

pub(crate) fn cache_home() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".cache"))
}

pub(crate) fn knewstuff_dir() -> PathBuf {
    data_home().join("knewstuff3")
}
