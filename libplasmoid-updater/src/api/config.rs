// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.kde-look.org/ocs/v1";
pub(crate) const DEFAULT_PAGE_SIZE: u8 = 100;
pub(crate) const DEFAULT_MAX_ATTEMPTS: u8 = 3;
pub(crate) const DEFAULT_INITIAL_BACKOFF_MS: u32 = 100;
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const MAX_DOWNLOAD_LINKS: usize = 64;

pub(crate) const USER_AGENT: &str = concat!("plasmoid-updater/", env!("CARGO_PKG_VERSION"));

/// Configuration for KDE Store API interactions.
pub(super) struct ApiConfig {
    pub(super) base_url: &'static str,
    pub(super) page_size: u8,
    pub(super) max_attempts: u8,
    pub(super) initial_backoff_ms: u32,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiConfig {
    pub(super) const fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL,
            page_size: DEFAULT_PAGE_SIZE,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
        }
    }
}

pub(crate) static DEFAULT_API_CONFIG: ApiConfig = ApiConfig::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_timeout_is_10_seconds() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn default_max_attempts_is_3() {
        assert_eq!(DEFAULT_MAX_ATTEMPTS, 3);
        assert_eq!(DEFAULT_API_CONFIG.max_attempts, 3);
    }
}
