// SPDX-License-Identifier: GPL-3.0-or-later
//
// OCS (Open Collaboration Services) XML parsing and extraction for KDE Store API responses.

use quick_xml::de::from_str;
use serde::{Deserialize, Deserializer};

use crate::{
    types::{ComponentType, DownloadLink, StoreEntry},
    {Error, Result},
};

use super::config::MAX_DOWNLOAD_LINKS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Ok,
    OkLegacy,
    RateLimited,
    Unknown(u16),
}

impl From<u16> for StatusCode {
    fn from(code: u16) -> Self {
        match code {
            100 => Self::Ok,
            0 => Self::OkLegacy,
            200 => Self::RateLimited,
            other => Self::Unknown(other),
        }
    }
}

impl StatusCode {
    pub(super) fn is_success(self) -> bool {
        matches!(self, Self::Ok | Self::OkLegacy)
    }

    pub(super) fn is_rate_limited(self) -> bool {
        matches!(self, Self::RateLimited)
    }

    pub(super) fn as_u16(self) -> u16 {
        match self {
            Self::Ok => 100,
            Self::OkLegacy => 0,
            Self::RateLimited => 200,
            Self::Unknown(code) => code,
        }
    }
}

/// A single content entry from the KDE Store OCS XML response.
#[derive(Debug)]
pub(super) struct ContentXml {
    id: u64,
    name: String,
    author: String,
    version: String,
    typeid: u16,
    changed: String,
    download_links: Vec<DownloadLink>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Meta {
    #[serde(rename = "statuscode")]
    pub status_code: StatusCode,
    #[serde(rename = "totalitems", default)]
    pub total_items: Option<u32>,
}

impl<'de> Deserialize<'de> for StatusCode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        u16::deserialize(deserializer).map(Self::from)
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct Data {
    #[serde(default)]
    pub content: Vec<ContentXml>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Response {
    pub meta: Meta,
    pub data: Data,
}

#[derive(Default)]
struct DownloadParts {
    url: Option<String>,
    version: Option<String>,
    checksum: Option<String>,
    size_kb: Option<u64>,
}

impl DownloadParts {
    fn into_link(self) -> Option<DownloadLink> {
        let url = self.url.filter(|u| !u.is_empty())?;
        Some(DownloadLink {
            url,
            version: self.version.unwrap_or_default(),
            checksum: self.checksum.filter(|s| !s.is_empty()),
            size_kb: self.size_kb,
        })
    }
}

fn try_parse_download_field<'de, A>(
    key: &str,
    downloads: &mut [DownloadParts; MAX_DOWNLOAD_LINKS],
    map: &mut A,
) -> std::result::Result<bool, A::Error>
where
    A: serde::de::MapAccess<'de>,
{
    if let Some(i) = parse_download_index(key, "downloadlink") {
        downloads[i].url = map.next_value()?;
        return Ok(true);
    }
    if let Some(i) = parse_download_index(key, "download_version") {
        downloads[i].version = map.next_value()?;
        return Ok(true);
    }
    if let Some(i) = parse_download_index(key, "downloadmd5sum") {
        downloads[i].checksum = map.next_value()?;
        return Ok(true);
    }
    if let Some(i) = parse_download_index(key, "downloadsize") {
        downloads[i].size_kb = map.next_value()?;
        return Ok(true);
    }
    Ok(false)
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
                let mut author = String::new();
                let mut version = String::new();
                let mut typeid: u16 = 0;
                let mut changed = String::new();
                let mut downloads: [DownloadParts; MAX_DOWNLOAD_LINKS] =
                    std::array::from_fn(|_| DownloadParts::default());

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => id = Some(map.next_value()?),
                        "name" => name = Some(map.next_value()?),
                        "personid" => author = map.next_value()?,
                        "version" => version = map.next_value()?,
                        "typeid" => typeid = map.next_value()?,
                        "changed" => changed = map.next_value()?,
                        _ => {
                            if !try_parse_download_field(&key, &mut downloads, &mut map)? {
                                let _ = map.next_value::<serde::de::IgnoredAny>()?;
                            }
                        }
                    }
                }

                Ok(ContentXml {
                    id: id.ok_or_else(|| serde::de::Error::missing_field("id"))?,
                    name: name.ok_or_else(|| serde::de::Error::missing_field("name"))?,
                    author,
                    version,
                    typeid,
                    changed,
                    download_links: downloads
                        .into_iter()
                        .filter_map(DownloadParts::into_link)
                        .collect(),
                })
            }
        }

        deserializer.deserialize_map(ContentXmlVisitor)
    }
}

fn parse_download_index(key: &str, prefix: &str) -> Option<usize> {
    let suffix = key.strip_prefix(prefix)?;
    let n = suffix.parse::<usize>().ok()?;
    if (1..=MAX_DOWNLOAD_LINKS).contains(&n) {
        Some(n - 1)
    } else {
        None
    }
}

impl ContentXml {
    pub(super) fn into_store_entry(self) -> StoreEntry {
        StoreEntry {
            id: self.id,
            name: self.name,
            author: self.author,
            version: self.version,
            type_id: self.typeid,
            download_links: self.download_links,
            changed_date: self.changed,
        }
    }
}

pub(crate) fn parse_ocs_response(xml: &str) -> Result<(Vec<StoreEntry>, Meta)> {
    let response: Response = from_str(xml).map_err(|e| Error::Parse {
        what: "ocs response",
        detail: e.to_string(),
    })?;

    if response.meta.status_code.is_rate_limited() {
        return Err(Error::RateLimited);
    }

    if !response.meta.status_code.is_success() {
        return Err(Error::Api {
            status: response.meta.status_code.as_u16(),
        });
    }

    let entries = response
        .data
        .content
        .into_iter()
        .map(ContentXml::into_store_entry)
        .collect();

    Ok((entries, response.meta))
}

/// The `categories=` parameter naming every category the given types list
/// entries under.
pub(crate) fn build_category_string(types: &[ComponentType]) -> String {
    let ids: usize = types.iter().map(|c| c.catalog_ids().len()).sum();
    // A five-digit id plus its `x` separator.
    let mut s = String::with_capacity(ids * 6);
    for (i, category) in types.iter().flat_map(|c| c.catalog_ids()).enumerate() {
        if i > 0 {
            s.push('x');
        }
        append_u16(&mut s, *category);
    }
    s
}

/// Appends the decimal digits of `n` directly into `s` without a heap allocation.
fn append_u16(s: &mut String, mut n: u16) {
    if n == 0 {
        s.push('0');
        return;
    }
    let mut buf = [0u8; 5];
    let mut pos = buf.len();
    while n > 0 {
        pos -= 1;
        buf[pos] = (n % 10) as u8 + b'0';
        n /= 10;
    }
    for &byte in &buf[pos..] {
        s.push(byte as char);
    }
}

#[cfg(test)]
mod tests {
    use super::{build_category_string, parse_ocs_response};

    #[test]
    fn a_widget_catalog_query_names_the_subcategory_the_parent_listing_skips() {
        // The store's 705 listing expands to 706-712 only; entries filed
        // under 713 need naming on their own (measured 2026-11-02).
        assert_eq!(
            build_category_string(&[crate::types::ComponentType::PlasmaWidget]),
            "705x713"
        );
    }

    #[test]
    fn a_multi_type_catalog_query_joins_every_types_categories() {
        assert_eq!(
            build_category_string(&[
                crate::types::ComponentType::PlasmaWidget,
                crate::types::ComponentType::ColorScheme,
            ]),
            "705x713x112"
        );
    }

    #[test]
    fn parses_the_uploader_username() {
        // The shape of a real /content/data/{id} answer: <personid> carries
        // the uploader's username, the only author field the store exposes.
        let xml = "<?xml version=\"1.0\"?><ocs><meta><status>ok</status>\
                   <statuscode>100</statuscode><message></message>\
                   <totalitems>1</totalitems><itemsperpage>100</itemsperpage></meta>\
                   <data><content details=\"full\"><id>2380745</id>\
                   <name>Claude Usage</name><version>0.2.0</version>\
                   <typeid>706</typeid><personid>Elwynn18</personid>\
                   <changed>2026-10-31T10:54:20+00:00</changed>\
                   <downloadlink1>https://example.org/a.plasmoid</downloadlink1>\
                   <download_version1>0.2.0</download_version1></content></data></ocs>";
        let (entries, _meta) = parse_ocs_response(xml).unwrap();
        assert_eq!(entries[0].author, "Elwynn18");
    }
}
