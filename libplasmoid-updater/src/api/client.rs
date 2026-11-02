// SPDX-License-Identifier: GPL-3.0-or-later
//
// API interaction based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{collections::HashSet, thread, time::Duration};

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
    /// Creates a new API client pointed at `config.api_url`.
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
            base_url: config.api_url.clone(),
        })
    }

    /// Returns a reference to the underlying HTTP client for reuse.
    pub fn http_client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// Fetches all content from specified categories with parallel page fetching.
    ///
    /// A later page that cannot be read is skipped and reported. Page 0 is
    /// different: it carries the total that paging needs, so when it fails
    /// after its retries the catalog is empty for this run, the warning says
    /// so, and only the per-id lookups of known ids can still resolve
    /// anything. Entries are deduplicated by content id because the catalog
    /// shifts under `sort=new` while later pages are in flight, and paging
    /// stops at [`API_CONFIG.max_pages`].
    pub fn fetch_all(&self, categories: &[ComponentType]) -> CatalogFetch {
        let category_str = build_category_string(categories);
        let page_size = u32::from(API_CONFIG.page_size);

        let first = self.fetch_page(&catalog_url(&self.base_url, &category_str, page_size, 0));
        let mut answered = first.answered;
        let Some(meta) = first.meta else {
            if let Some(error) = &first.error {
                log::warn!(
                    target: "api",
                    "catalog page 0 unavailable, so the catalog is empty for this run: {error}"
                );
            }
            return CatalogFetch {
                entries: Vec::new(),
                answered,
                error: first.error,
            };
        };

        let Some(total_items) = meta.total_items else {
            // Without a total the page count is unknown; page 0 is still a real
            // answer, so keep what it listed and stop instead of guessing.
            log::warn!(
                target: "api",
                "catalog page 0 has no totalitems; keeping its {} entries and fetching no further pages",
                first.entries.len()
            );
            return CatalogFetch {
                entries: first.entries,
                answered,
                error: None,
            };
        };

        let wanted_pages = total_items.div_ceil(page_size);
        let total_pages = wanted_pages.min(API_CONFIG.max_pages);
        if wanted_pages > total_pages {
            log::warn!(
                target: "api",
                "store reports {total_items} items ({wanted_pages} pages); fetching {total_pages} of them"
            );
        }

        let remaining_pages: Vec<u32> = (1..total_pages).collect();
        let results: Vec<(u32, Page)> = remaining_pages
            .par_iter()
            .map(|&page| {
                (
                    page,
                    self.fetch_page(&catalog_url(&self.base_url, &category_str, page_size, page)),
                )
            })
            .collect();

        let mut entries = first.entries;
        for (page, fetched) in results {
            answered |= fetched.answered;
            if let Some(error) = &fetched.error {
                log::warn!(target: "api", "catalog page {page} unavailable, skipping it: {error}");
                continue;
            }
            entries.extend(fetched.entries);
        }

        let mut seen = HashSet::with_capacity(entries.len());
        entries.retain(|entry| seen.insert(entry.id));

        CatalogFetch {
            entries,
            answered,
            error: None,
        }
    }

    /// Fetches content details of multiple components.
    pub fn fetch_details(&self, content_ids: &[u64]) -> Vec<DetailFetch> {
        content_ids
            .par_iter()
            .map(|&id| {
                let url = format!("{}/content/data/{id}", self.base_url);
                let page = self.fetch_page(&url);
                let answered = page.answered;
                let result = match page.error {
                    Some(error) => Err(error),
                    None => page.entries.into_iter().next().ok_or_else(|| Error::Parse {
                        what: "store entry",
                        detail: format!("content id {id}: empty data"),
                    }),
                };
                DetailFetch { result, answered }
            })
            .collect()
    }

    /// Fetches one page, recording whether the store answered it at all.
    fn fetch_page(&self, url: &str) -> Page {
        let mut answered = false;
        let result = self.request_page(url, &mut answered);
        Page::from_result(result, answered)
    }

    fn request_page(&self, url: &str, answered: &mut bool) -> Result<(Vec<StoreEntry>, Meta)> {
        let mut backoff_ms = API_CONFIG.initial_backoff_ms;
        let attempts = API_CONFIG.max_retries;

        for attempt in 0..attempts {
            let last_attempt = attempt + 1 >= attempts;

            let r = match self.client.get(url).send() {
                Ok(response) => {
                    *answered = true;
                    response
                }
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

/// The catalog entries one check could read, plus whether the store answered
/// any of its page requests at all.
pub(crate) struct CatalogFetch {
    pub(crate) entries: Vec<StoreEntry>,
    /// True when at least one page got an HTTP response, even a bad one.
    pub(crate) answered: bool,
    /// The page-0 error when no page could be read, for a run that cannot
    /// tell "empty catalog" from "store unreachable".
    pub(crate) error: Option<Error>,
}

/// One per-id lookup's outcome, plus whether the store answered it at all.
pub(crate) struct DetailFetch {
    pub(crate) result: Result<StoreEntry>,
    /// True when the store sent an HTTP response, even one whose body then
    /// broke off.
    pub(crate) answered: bool,
}

/// One catalog page's outcome. Reading a page never fails the run: the page
/// carries its error and the caller decides whether to skip it or give up.
struct Page {
    entries: Vec<StoreEntry>,
    meta: Option<Meta>,
    error: Option<Error>,
    answered: bool,
}

impl Page {
    fn from_result(result: Result<(Vec<StoreEntry>, Meta)>, answered: bool) -> Self {
        match result {
            Ok((entries, meta)) => Self {
                entries,
                meta: Some(meta),
                error: None,
                answered,
            },
            Err(error) => Self {
                entries: Vec::new(),
                meta: None,
                error: Some(error),
                answered,
            },
        }
    }
}

fn catalog_url(base_url: &str, categories: &str, page_size: u32, page: u32) -> String {
    format!(
        "{base_url}/content/data?categories={categories}&page={page}&pagesize={page_size}&sort=new"
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn entry(id: u64) -> String {
        format!(
            "<content><id>{id}</id><name>Item {id}</name><version>2.0</version>\
             <typeid>705</typeid><changed>2026-10-01</changed></content>"
        )
    }

    fn ocs(total: Option<u32>, content: &str) -> String {
        let total_items = total
            .map(|items| format!("<totalitems>{items}</totalitems>"))
            .unwrap_or_default();
        format!(
            "<?xml version=\"1.0\"?>\n<ocs><meta><status>ok</status><statuscode>100</statuscode>\
             <message></message>{total_items}<itemsperpage>100</itemsperpage>\
             </meta><data>{content}</data></ocs>"
        )
    }

    fn page_of(target: &str) -> usize {
        target
            .split('&')
            .find_map(|kv| kv.strip_prefix("page="))
            .and_then(|page| page.parse().ok())
            .unwrap_or(0)
    }

    /// Serves `bodies[page]` for each catalog page request, and counts them.
    fn serve(bodies: Vec<String>) -> (String, Arc<AtomicUsize>) {
        let bodies = Arc::new(bodies);
        let requests = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&requests);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let bodies = Arc::clone(&bodies);
                let served = Arc::clone(&served);
                std::thread::spawn(move || respond(stream, &bodies, &served));
            }
        });
        (format!("http://127.0.0.1:{port}/ocs/v1"), requests)
    }

    fn respond(mut stream: TcpStream, bodies: &[String], requests: &AtomicUsize) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) | Err(_) => break,
                Ok(_) if header == "\r\n" || header == "\n" => break,
                Ok(_) => {}
            }
        }
        let target = request_line.split_whitespace().nth(1).unwrap_or("");
        requests.fetch_add(1, Ordering::SeqCst);
        let body = bodies.get(page_of(target)).cloned().unwrap_or_default();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    }

    fn client_for(base_url: String) -> ApiClient {
        ApiClient::new(&Config::new().with_api_url(base_url)).unwrap()
    }

    #[test]
    fn catalog_pages_are_deduplicated_by_content_id() {
        let (base, _requests) = serve(vec![
            ocs(Some(101), &entry(1)),
            ocs(Some(101), &format!("{}{}", entry(1), entry(2))),
        ]);
        let catalog = client_for(base).fetch_all(&[ComponentType::PlasmaWidget]);
        let ids: Vec<u64> = catalog.entries.iter().map(|item| item.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(catalog.answered);
    }

    #[test]
    fn catalog_page_without_totalitems_keeps_its_entries_and_fetches_no_more_pages() {
        let (base, requests) = serve(vec![ocs(None, &entry(1))]);
        let catalog = client_for(base).fetch_all(&[ComponentType::PlasmaWidget]);
        let ids: Vec<u64> = catalog.entries.iter().map(|item| item.id).collect();
        assert_eq!(ids, vec![1]);
        assert!(catalog.answered);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "an unknown total must not drive further page requests"
        );
    }

    #[test]
    fn a_failed_first_page_leaves_the_catalog_empty_and_asks_for_no_more() {
        let (base, _requests) = serve(vec![
            "<html>maintenance</html>".to_string(),
            ocs(Some(200), &entry(1)),
        ]);
        let catalog = client_for(base).fetch_all(&[ComponentType::PlasmaWidget]);
        assert!(
            catalog.entries.is_empty(),
            "without page 0 there is no total, so page 1 must not be read"
        );
        assert!(catalog.answered, "a garbage page is still an answer");
        assert!(catalog.error.is_some());
    }

    #[test]
    fn a_bad_page_after_the_first_does_not_stop_later_pages() {
        let (base, _requests) = serve(vec![
            ocs(Some(300), &entry(1)),
            "<html>maintenance</html>".to_string(),
            ocs(Some(300), &entry(3)),
        ]);
        let catalog = client_for(base).fetch_all(&[ComponentType::PlasmaWidget]);
        let ids: Vec<u64> = catalog.entries.iter().map(|item| item.id).collect();
        assert_eq!(ids, vec![1, 3]);
        assert!(catalog.answered);
    }
}
