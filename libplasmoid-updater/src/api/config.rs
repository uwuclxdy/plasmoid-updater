// SPDX-License-Identifier: MIT OR Apache-2.0

use std::time::Duration;

pub(crate) const DEFAULT_BASE_URL: &str = "https://api.kde-look.org/ocs/v1";
pub(crate) const DEFAULT_PAGE_SIZE: u8 = 100;
pub(crate) const DEFAULT_MAX_RETRIES: u8 = 3;
pub(crate) const DEFAULT_INITIAL_BACKOFF_MS: u8 = 100;
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const MAX_DOWNLOAD_LINKS: usize = 64;

pub const USER_AGENT: &str = concat!("plasmoid-updater/", env!("CARGO_PKG_VERSION"));

/// Configuration for KDE Store API interactions.
pub struct ApiConfig {
    pub base_url: &'static str,
    pub page_size: u8,
    pub max_retries: u8,
    pub initial_backoff_ms: u8,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiConfig {
    pub const fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL,
            page_size: DEFAULT_PAGE_SIZE,
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
        }
    }
}

pub(crate) static DEFAULT_API_CONFIG: ApiConfig = ApiConfig::new();
