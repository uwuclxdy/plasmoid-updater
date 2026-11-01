// SPDX-License-Identifier: GPL-3.0-or-later
//
// API interaction based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{thread, time::Duration};

use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

use crate::{
    Config, Error, Result,
    types::{ComponentType, StoreEntry},
};

use super::config::{API_CONFIG, CONNECT_TIMEOUT, REQUEST_TIMEOUT, USER_AGENT};
use super::ocs_parser::Meta;
use super::ocs_parser::{build_category_string, parse_ocs_response};

/// Thread-safe API client for KDE Store interactions.
///
/// Every request targets the OCS base URL the run's [`Config`] carries.
#[derive(Clone)]
pub(crate) struct ApiClient {
    client: reqwest::blocking::Client,
    base_url: String,
}

impl ApiClient {
    /// Creates a new API client pointed at `config.store_url`.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be created (for example, if
    /// the TLS backend is unavailable).
    pub fn new(config: &Config) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;

        Ok(Self {
            client,
            base_url: config.store_url.clone(),
        })
    }

    /// Returns a reference to the underlying HTTP client for reuse.
    pub fn http_client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// Fetches all content from specified categories with parallel page fetching.
    pub fn fetch_all(&self, categories: &[ComponentType]) -> Result<Vec<StoreEntry>> {
        let category_str = build_category_string(categories);
        let base_url = &self.base_url;
        let page_size = u32::from(API_CONFIG.page_size);

        let first_url = format!(
            "{base_url}/content/data?categories={category_str}&page=0&pagesize={page_size}&sort=new"
        );

        let (first_entries, meta) = self.fetch_page(&first_url)?;
        let total_items = meta.total_items;

        if total_items <= page_size {
            return Ok(first_entries);
        }

        let total_pages = total_items.div_ceil(page_size);

        if total_pages > 10 {
            log::warn!(
                target: "api",
                "store returned {total_items} items across {total_pages} pages; fetch may be slow"
            );
        }

        let remaining_pages: Vec<u32> = (1..total_pages).collect();

        let results: Vec<Result<(Vec<StoreEntry>, _)>> = remaining_pages
            .par_iter()
            .map(|&page| {
                let url = format!(
                    "{base_url}/content/data?categories={category_str}&page={page}&pagesize={page_size}&sort=new"
                );
                self.fetch_page(&url)
            })
            .collect();

        let mut all_entries = first_entries;
        for (entries, _) in results.into_iter().collect::<Result<Vec<_>>>()? {
            all_entries.extend(entries);
        }

        Ok(all_entries)
    }

    /// Fetches content details of multiple components.
    pub fn fetch_details(&self, content_ids: &[u64]) -> Vec<Result<StoreEntry>> {
        content_ids
            .par_iter()
            .map(|&id| {
                let url = format!("{}/content/data/{id}", self.base_url);
                let (entries, _) = self.fetch_page(&url)?;
                entries.into_iter().next().ok_or_else(|| Error::Parse {
                    what: "store entry",
                    detail: format!("content id {id}: empty data"),
                })
            })
            .collect()
    }

    fn fetch_page(&self, url: &str) -> Result<(Vec<StoreEntry>, Meta)> {
        let mut backoff_ms = API_CONFIG.initial_backoff_ms;
        let attempts = API_CONFIG.max_retries;

        for attempt in 0..attempts {
            let last_attempt = attempt + 1 >= attempts;

            let r = match self.client.get(url).send() {
                Ok(response) => response,
                Err(e) if !last_attempt => {
                    log::warn!(
                        target: "api",
                        "request failed (attempt {} of {attempts}): {e}",
                        attempt + 1
                    );
                    backoff_sleep(&mut backoff_ms);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let retry_after_secs = parse_retry_after(&r);
            let status = r.status();

            // HTTP 429: respect Retry-After with a single retry.
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return match retry_after_secs {
                    Some(secs) => self.send_after(url, secs),
                    None => Err(Error::RateLimited),
                };
            }

            // A timeout or a server error is transient and gets the same
            // backoff as a bad payload; any other non-success status is a
            // deterministic answer, not a payload to parse, so it surfaces as
            // Api on the spot.
            if !status.is_success() {
                if (status == reqwest::StatusCode::REQUEST_TIMEOUT || status.is_server_error())
                    && !last_attempt
                {
                    log::warn!(
                        target: "api",
                        "store returned {status} (attempt {} of {attempts})",
                        attempt + 1
                    );
                    backoff_sleep(&mut backoff_ms);
                    continue;
                }
                return Err(Error::Api {
                    status: status.as_u16(),
                });
            }

            let xml = match r.text() {
                Ok(xml) => xml,
                Err(e) if !last_attempt => {
                    log::warn!(
                        target: "api",
                        "response body failed (attempt {} of {attempts}): {e}",
                        attempt + 1
                    );
                    backoff_sleep(&mut backoff_ms);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            match parse_ocs_response(&xml) {
                Ok(result) => return Ok(result),
                Err(Error::RateLimited) => {
                    if let Some(secs) = retry_after_secs {
                        return self.send_after(url, secs);
                    }
                    if !last_attempt {
                        backoff_sleep(&mut backoff_ms);
                        continue;
                    }
                    return Err(Error::RateLimited);
                }
                // An Api status is a deterministic OCS answer — retrying wastes a request.
                Err(e @ Error::Api { .. }) => return Err(e),
                Err(e) => {
                    if last_attempt {
                        return Err(e);
                    }
                    backoff_sleep(&mut backoff_ms);
                }
            }
        }

        // A zero retry count skips the loop entirely; name that state instead
        // of panicking on it.
        Err(Error::Config(format!(
            "store request retry count is zero (max_retries = {attempts})"
        )))
    }

    /// Sleeps for `secs` then sends one retry request.
    fn send_after(&self, url: &str, secs: u64) -> Result<(Vec<StoreEntry>, Meta)> {
        log::info!(target: "api", "rate limited, retrying after {secs}s");
        thread::sleep(Duration::from_secs(secs));
        let r = self.client.get(url).send()?;

        if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::RateLimited);
        }

        if !r.status().is_success() {
            return Err(Error::Api {
                status: r.status().as_u16(),
            });
        }

        let xml = r.text()?;
        parse_ocs_response(&xml)
    }
}

/// Sleeps for the current backoff, then doubles it for the next attempt.
fn backoff_sleep(backoff_ms: &mut u32) {
    thread::sleep(Duration::from_millis(u64::from(*backoff_ms)));
    *backoff_ms = backoff_ms.saturating_mul(2);
}

fn parse_retry_after(response: &reqwest::blocking::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}
