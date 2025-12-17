// SPDX-License-Identifier: MIT OR Apache-2.0
//
// API interaction based on Apdatifier (https://github.com/exequtic/apdatifier) - MIT License
// and KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::{sync::Arc, thread, time::Duration};

use parking_lot::Mutex;
use quick_xml::de::from_str;
use rayon::prelude::*;
use serde::{Deserialize, Deserializer};

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
        let category_str = build_category_string(categories);

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

#[derive(Debug)]
struct ContentXml {
    id: u64,
    name: String,
    version: String,
    typeid: u16,
    changed: String,
    download_links: Vec<DownloadLink>,
}

#[derive(Default)]
struct DownloadParts {
    url: Option<String>,
    version: Option<String>,
    checksum: Option<String>,
    size_kb: Option<u64>,
}

impl<'de> Deserialize<'de> for ContentXml {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ContentXmlVisitor;

        impl<'de> serde::de::Visitor<'de> for ContentXmlVisitor {
            type Value = ContentXml;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("kde store content xml")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut id: Option<u64> = None;
                let mut name: Option<String> = None;
                let mut version: String = String::new();
                let mut typeid: u16 = 0;
                let mut changed: String = String::new();

                let mut downloads: [DownloadParts; 10] =
                    std::array::from_fn(|_| DownloadParts::default());

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => id = Some(map.next_value::<u64>()?),
                        "name" => name = Some(map.next_value::<String>()?),
                        "version" => version = map.next_value::<String>()?,
                        "typeid" => typeid = map.next_value::<u16>()?,
                        "changed" => changed = map.next_value::<String>()?,
                        _ => {
                            if let Some(index) = parse_download_index(&key, "downloadlink") {
                                downloads[index].url = map.next_value::<Option<String>>()?;
                                continue;
                            }
                            if let Some(index) = parse_download_index(&key, "download_version") {
                                downloads[index].version = map.next_value::<Option<String>>()?;
                                continue;
                            }
                            if let Some(index) = parse_download_index(&key, "downloadmd5sum") {
                                downloads[index].checksum = map.next_value::<Option<String>>()?;
                                continue;
                            }
                            if let Some(index) = parse_download_index(&key, "downloadsize") {
                                downloads[index].size_kb = map.next_value::<Option<u64>>()?;
                                continue;
                            }

                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }

                let id = id.ok_or_else(|| serde::de::Error::missing_field("id"))?;
                let name = name.ok_or_else(|| serde::de::Error::missing_field("name"))?;

                let mut download_links = Vec::new();
                for part in downloads {
                    let Some(url) = part.url else {
                        continue;
                    };
                    if url.is_empty() {
                        continue;
                    }
                    download_links.push(DownloadLink {
                        url,
                        version: part.version.unwrap_or_default(),
                        checksum: part.checksum.filter(|s| !s.is_empty()),
                        size_kb: part.size_kb,
                    });
                }

                Ok(ContentXml {
                    id,
                    name,
                    version,
                    typeid,
                    changed,
                    download_links,
                })
            }
        }

        deserializer.deserialize_map(ContentXmlVisitor)
    }
}

fn parse_download_index(key: &str, prefix: &str) -> Option<usize> {
    if !key.starts_with(prefix) {
        return None;
    }

    let suffix = &key[prefix.len()..];
    let Ok(n) = suffix.parse::<usize>() else {
        return None;
    };
    if (1..=10).contains(&n) {
        Some(n - 1)
    } else {
        None
    }
}

impl ContentXml {
    fn into_store_entry(self) -> StoreEntry {
        StoreEntry {
            id: self.id,
            name: self.name,
            version: self.version,
            type_id: self.typeid,
            download_links: self.download_links,
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
