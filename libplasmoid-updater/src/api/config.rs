// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

pub(crate) const DEFAULT_PAGE_SIZE: u8 = 100;
pub(crate) const DEFAULT_MAX_RETRIES: u8 = 3;
pub(crate) const DEFAULT_INITIAL_BACKOFF_MS: u32 = 100;
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const MAX_DOWNLOAD_LINKS: usize = 64;

pub(crate) const USER_AGENT: &str = concat!("plasmoid-updater/", env!("CARGO_PKG_VERSION"));

/// Tuning knobs for KDE Store API interactions, minus the base URL: that one
/// comes from [`crate::Config::store_url`] per run.
pub(super) struct ApiConfig {
    pub(super) page_size: u8,
    pub(super) max_retries: u8,
    pub(super) initial_backoff_ms: u32,
}

pub(crate) static API_CONFIG: ApiConfig = ApiConfig {
    page_size: DEFAULT_PAGE_SIZE,
    max_retries: DEFAULT_MAX_RETRIES,
    initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_timeout_is_10_seconds() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn default_max_retries_is_3() {
        assert_eq!(DEFAULT_MAX_RETRIES, 3);
        assert_eq!(API_CONFIG.max_retries, 3);
    }
}
