// SPDX-License-Identifier: MIT OR Apache-2.0
//
// API interaction based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::{sync::Arc, thread, time::Duration};

use parking_lot::Mutex;
use quick_xml::de::from_str;
use rayon::prelude::*;
use serde::Deserialize;

use crate::{ComponentType, DownloadLink, Error, Result, StoreEntry};

const API_BASE_URL: &str = "https://api.kde-look.org/ocs/v1";
const USER_AGENT: &str = concat!("plasmoid-updater/", env!("CARGO_PKG_VERSION"));
const PAGE_SIZE: u16 = 100;
const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 100;

/// thread-safe API client that can be shared across threads.
#[derive(Clone)]
pub struct ApiClient {
    client: reqwest::blocking::Client,
}

impl Default for ApiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiClient {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to create http client");

        Self { client }
    }

    /// returns a reference to the underlying HTTP client for reuse.
    pub fn http_client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// fetches all content from specified categories with parallel page fetching.
    pub fn fetch_all_content(&self, categories: &[ComponentType]) -> Result<Vec<StoreEntry>> {
        let category_str = categories
            .iter()
            .map(|c| c.category_id().to_string())
            .collect::<Vec<_>>()
            .join("x");

        // first fetch to get total items
        let first_url = format!(
            "{API_BASE_URL}/content/data?categories={category_str}&page=0&pagesize={PAGE_SIZE}&sort=new"
        );

        let (first_entries, meta) = self.fetch_page_with_retry(&first_url)?;
        let total_items = meta.total_items;

        if total_items <= u32::from(PAGE_SIZE) {
            return Ok(first_entries);
        }

        // calculate remaining pages
        let total_pages = total_items.div_ceil(u32::from(PAGE_SIZE));
        let remaining_pages: Vec<u32> = (1..total_pages).collect();

        // fetch remaining pages in parallel
        let all_entries = Arc::new(Mutex::new(first_entries));
        let errors = Arc::new(Mutex::new(Vec::new()));

        remaining_pages.par_iter().for_each(|&page| {
            let url = format!(
                "{API_BASE_URL}/content/data?categories={category_str}&page={page}&pagesize={PAGE_SIZE}&sort=new"
            );

            match self.fetch_page_with_retry(&url) {
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
            log::warn!("**api:** {} page(s) failed to fetch", errors.len());
        }

        Ok(Arc::try_unwrap(all_entries).unwrap().into_inner())
    }

    pub fn fetch_content_details(&self, content_id: u64) -> Result<StoreEntry> {
        let url = format!("{API_BASE_URL}/content/data/{content_id}");
        let (entries, _) = self.fetch_page_with_retry(&url)?;
        entries
            .into_iter()
            .next()
            .ok_or_else(|| Error::other(format!("no content found for id {content_id}")))
    }

    /// fetches multiple content details in parallel.
    pub fn fetch_content_details_batch(&self, content_ids: &[u64]) -> Vec<Result<StoreEntry>> {
        content_ids
            .par_iter()
            .map(|&id| self.fetch_content_details(id))
            .collect()
    }

    fn fetch_page_with_retry(&self, url: &str) -> Result<(Vec<StoreEntry>, OcsMeta)> {
        let mut backoff_ms = INITIAL_BACKOFF_MS;

        for attempt in 0..MAX_RETRIES {
            return match self.fetch_page(url) {
                Ok(result) => Ok(result),
                Err(Error::RateLimited) => {
                    if attempt + 1 < MAX_RETRIES {
                        thread::sleep(Duration::from_millis(backoff_ms));
                        backoff_ms *= 2;
                        continue;
                    }
                    Err(Error::RateLimited)
                }
                Err(e) => {
                    if attempt + 1 < MAX_RETRIES {
                        thread::sleep(Duration::from_millis(backoff_ms));
                        backoff_ms *= 2;
                        continue;
                    }
                    Err(e)
                }
            };
        }

        Err(Error::other("max retries exceeded"))
    }

    fn fetch_page(&self, url: &str) -> Result<(Vec<StoreEntry>, OcsMeta)> {
        let response = self.client.get(url).send()?;
        let xml = response.text()?;
        parse_ocs_response(&xml)
    }
}

#[derive(Debug, Clone)]
pub struct OcsMeta {
    pub status_code: u16,
    pub total_items: u32,
}

#[derive(Debug, Deserialize)]
struct OcsResponse {
    meta: OcsMetaXml,
    data: OcsData,
}

#[derive(Debug, Deserialize)]
struct OcsMetaXml {
    statuscode: u16,
    #[serde(default)]
    totalitems: u32,
}

#[derive(Debug, Deserialize)]
struct OcsData {
    #[serde(default)]
    content: Vec<ContentXml>,
}

#[derive(Debug, Deserialize)]
struct ContentXml {
    id: u64,
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    typeid: u16,
    #[serde(default)]
    changed: String,
    #[serde(default)]
    downloadlink1: Option<String>,
    #[serde(default, rename = "download_version1")]
    downloadversion1: Option<String>,
    #[serde(default)]
    downloadmd5sum1: Option<String>,
    #[serde(default)]
    downloadsize1: Option<u64>,
    #[serde(default)]
    downloadlink2: Option<String>,
    #[serde(default, rename = "download_version2")]
    downloadversion2: Option<String>,
    #[serde(default)]
    downloadmd5sum2: Option<String>,
    #[serde(default)]
    downloadsize2: Option<u64>,
    #[serde(default)]
    downloadlink3: Option<String>,
    #[serde(default, rename = "download_version3")]
    downloadversion3: Option<String>,
    #[serde(default)]
    downloadmd5sum3: Option<String>,
    #[serde(default)]
    downloadsize3: Option<u64>,
    #[serde(default)]
    downloadlink4: Option<String>,
    #[serde(default, rename = "download_version4")]
    downloadversion4: Option<String>,
    #[serde(default)]
    downloadmd5sum4: Option<String>,
    #[serde(default)]
    downloadsize4: Option<u64>,
    #[serde(default)]
    downloadlink5: Option<String>,
    #[serde(default, rename = "download_version5")]
    downloadversion5: Option<String>,
    #[serde(default)]
    downloadmd5sum5: Option<String>,
    #[serde(default)]
    downloadsize5: Option<u64>,
    #[serde(default)]
    downloadlink6: Option<String>,
    #[serde(default, rename = "download_version6")]
    downloadversion6: Option<String>,
    #[serde(default)]
    downloadmd5sum6: Option<String>,
    #[serde(default)]
    downloadsize6: Option<u64>,
    #[serde(default)]
    downloadlink7: Option<String>,
    #[serde(default, rename = "download_version7")]
    downloadversion7: Option<String>,
    #[serde(default)]
    downloadmd5sum7: Option<String>,
    #[serde(default)]
    downloadsize7: Option<u64>,
    #[serde(default)]
    downloadlink8: Option<String>,
    #[serde(default, rename = "download_version8")]
    downloadversion8: Option<String>,
    #[serde(default)]
    downloadmd5sum8: Option<String>,
    #[serde(default)]
    downloadsize8: Option<u64>,
    #[serde(default)]
    downloadlink9: Option<String>,
    #[serde(default, rename = "download_version9")]
    downloadversion9: Option<String>,
    #[serde(default)]
    downloadmd5sum9: Option<String>,
    #[serde(default)]
    downloadsize9: Option<u64>,
    #[serde(default)]
    downloadlink10: Option<String>,
    #[serde(default, rename = "download_version10")]
    downloadversion10: Option<String>,
    #[serde(default)]
    downloadmd5sum10: Option<String>,
    #[serde(default)]
    downloadsize10: Option<u64>,
}

impl ContentXml {
    fn into_store_entry(self) -> StoreEntry {
        type LinkRef<'a> = (
            &'a Option<String>,
            &'a Option<String>,
            &'a Option<String>,
            &'a Option<u64>,
        );
        let mut download_links = Vec::new();

        let links: [LinkRef<'_>; 10] = [
            (
                &self.downloadlink1,
                &self.downloadversion1,
                &self.downloadmd5sum1,
                &self.downloadsize1,
            ),
            (
                &self.downloadlink2,
                &self.downloadversion2,
                &self.downloadmd5sum2,
                &self.downloadsize2,
            ),
            (
                &self.downloadlink3,
                &self.downloadversion3,
                &self.downloadmd5sum3,
                &self.downloadsize3,
            ),
            (
                &self.downloadlink4,
                &self.downloadversion4,
                &self.downloadmd5sum4,
                &self.downloadsize4,
            ),
            (
                &self.downloadlink5,
                &self.downloadversion5,
                &self.downloadmd5sum5,
                &self.downloadsize5,
            ),
            (
                &self.downloadlink6,
                &self.downloadversion6,
                &self.downloadmd5sum6,
                &self.downloadsize6,
            ),
            (
                &self.downloadlink7,
                &self.downloadversion7,
                &self.downloadmd5sum7,
                &self.downloadsize7,
            ),
            (
                &self.downloadlink8,
                &self.downloadversion8,
                &self.downloadmd5sum8,
                &self.downloadsize8,
            ),
            (
                &self.downloadlink9,
                &self.downloadversion9,
                &self.downloadmd5sum9,
                &self.downloadsize9,
            ),
            (
                &self.downloadlink10,
                &self.downloadversion10,
                &self.downloadmd5sum10,
                &self.downloadsize10,
            ),
        ];

        for (link, version, checksum, size) in links {
            if let Some(url) = link
                && !url.is_empty()
            {
                download_links.push(DownloadLink {
                    url: url.clone(),
                    version: version.clone().unwrap_or_default(),
                    checksum: checksum.clone().filter(|s| !s.is_empty()),
                    size_kb: *size,
                });
            }
        }

        StoreEntry {
            id: self.id,
            name: self.name,
            version: self.version,
            type_id: self.typeid,
            download_links,
            changed_date: self.changed,
        }
    }
}

pub(crate) fn parse_ocs_response(xml: &str) -> Result<(Vec<StoreEntry>, OcsMeta)> {
    let response: OcsResponse =
        from_str(xml).map_err(|e| Error::xml_parse(format!("xml parse error: {e}")))?;

    let meta = OcsMeta {
        status_code: response.meta.statuscode,
        total_items: response.meta.totalitems,
    };

    if meta.status_code == 200 {
        return Err(Error::RateLimited);
    }

    if meta.status_code != 100 && meta.status_code != 0 {
        return Err(Error::ApiError(meta.status_code));
    }

    let entries = response
        .data
        .content
        .into_iter()
        .map(ContentXml::into_store_entry)
        .collect();

    Ok((entries, meta))
}

pub fn build_category_string(types: &[ComponentType]) -> String {
    types
        .iter()
        .map(|c| c.category_id().to_string())
        .collect::<Vec<_>>()
        .join("x")
}
