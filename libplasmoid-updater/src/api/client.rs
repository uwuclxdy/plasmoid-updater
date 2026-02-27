// SPDX-License-Identifier: MIT OR Apache-2.0
//
// API interaction based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::{sync::Arc, thread, time::Duration};

use parking_lot::Mutex;
use rayon::prelude::*;

use crate::{
    types::{ComponentType, StoreEntry},
    {Error, Result},
};

use super::config::{ApiConfig, CONNECT_TIMEOUT, DEFAULT_API_CONFIG, REQUEST_TIMEOUT, USER_AGENT};
use super::ocs_parser::Meta;
use super::ocs_parser::{build_category_string, parse_ocs_response};

/// Thread-safe API client for KDE Store interactions.
#[derive(Clone)]
pub(crate) struct ApiClient {
    client: reqwest::blocking::Client,
    config: &'static ApiConfig,
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiClient {
    /// Creates a new API client with default configuration.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be created (e.g., TLS backend unavailable).
    pub fn new() -> Self {
        Self::with_config(&DEFAULT_API_CONFIG)
            .unwrap_or_else(|e| panic!("failed to create API client: {e}"))
    }

    /// Creates a new API client with the given configuration.
    pub(super) fn with_config(config: &'static ApiConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        Ok(Self { client, config })
    }

    /// Returns a reference to the underlying HTTP client for reuse.
    pub fn http_client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// Fetches all content from specified categories with parallel page fetching.
    pub fn fetch_all(&self, categories: &[ComponentType]) -> Result<Vec<StoreEntry>> {
        let category_str = build_category_string(categories);
        let base_url = self.config.base_url;
        let page_size = self.config.page_size;

        let first_url = format!(
            "{base_url}/content/data?categories={category_str}&page=0&pagesize={page_size}&sort=new"
        );

        let (first_entries, meta) = self.fetch_page(&first_url)?;
        let total_items = meta.total_items;

        if total_items <= u32::from(page_size) {
            return Ok(first_entries);
        }

        let total_pages = total_items.div_ceil(u32::from(page_size));
        let remaining_pages: Vec<u32> = (1..total_pages).collect();

        let all_entries = Arc::new(Mutex::new(first_entries));
        let errors = Arc::new(Mutex::new(Vec::new()));

        remaining_pages.par_iter().for_each(|&page| {
            let url = format!(
                "{base_url}/content/data?categories={category_str}&page={page}&pagesize={page_size}&sort=new"
            );

            match self.fetch_page(&url) {
                Ok((entries, _)) => {
                    all_entries.lock().extend(entries);
                }
                Err(e) => {
                    errors.lock().push(e);
                }
            }
        });

        let errors = Arc::try_unwrap(errors).unwrap().into_inner();
        if !errors.is_empty() {
            log::warn!(target: "api", "{} page{} failed to fetch", errors.len(), if errors.len() == 1 { "" } else { "s" });
        }

        Ok(Arc::try_unwrap(all_entries).unwrap().into_inner())
    }

    /// Fetches content details of multiple components.
    pub fn fetch_details(&self, content_ids: &[u64]) -> Vec<Result<StoreEntry>> {
        content_ids
            .par_iter()
            .map(|&id| {
                let base_url = self.config.base_url;
                let url = format!("{base_url}/content/data/{id}");
                let (entries, _) = self.fetch_page(&url)?;
                entries
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::ComponentNotFound(format!("store content id {id}")))
            })
            .collect()
    }

    fn fetch_page(&self, url: &str) -> Result<(Vec<StoreEntry>, Meta)> {
        let mut backoff_ms = self.config.initial_backoff_ms;

        for attempt in 0..self.config.max_retries {
            let response = {
                let r = self.client.get(url).send()?;
                let xml = r.text()?;
                parse_ocs_response(&xml)
            };
            match response {
                Ok(result) => return Ok(result),
                Err(_) if attempt + 1 < self.config.max_retries => {
                    thread::sleep(Duration::from_millis(backoff_ms.into()));
                    backoff_ms = backoff_ms.saturating_mul(2);
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::other("max retries exceeded"))
    }
}
