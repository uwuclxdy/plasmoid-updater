// SPDX-License-Identifier: GPL-3.0-or-later
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use quick_xml::{Reader, Writer, events::Event};

use crate::{
    types::ComponentType,
    {Error, Result},
};

use super::{manager::RegistryEntry, utils};

/// Raw fields collected from a single `<stuff>` entry during XML parsing.
#[derive(Default)]
pub(super) struct RawEntry {
    name: String,
    version: String,
    id_text: String,
    release_date: String,
    installed_files: Vec<String>,
    uninstalled_files: Vec<String>,
}

impl RawEntry {
    pub(super) fn content_id(&self) -> Option<u64> {
        self.id_text.parse().ok()
    }

    pub(super) fn first_installed_path(&self) -> Option<PathBuf> {
        self.installed_files
            .first()
            .map(|f| PathBuf::from(f.trim_end_matches("/*")))
    }
}

/// Entry data for adding to the registry.
pub(super) struct NewEntry<'a> {
    pub name: &'a str,
    pub component_type: ComponentType,
    pub content_id: u64,
    pub version: &'a str,
    pub download_url: &'a str,
    pub installed_path: &'a Path,
    pub release_date: &'a str,
}

/// Fields to update in a registry entry.
pub(super) struct UpdateFields<'a> {
    pub directory_name: &'a str,
    pub content_id: u64,
    pub new_version: &'a str,
    pub download_url: &'a str,
    pub installed_path: &'a Path,
    pub release_date: &'a str,
}

const EMPTY_REGISTRY_TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
</hotnewstuffregistry>
"#;

/// Parses all `<stuff>` entries from registry XML into raw field collections.
pub(super) fn parse_raw_entries(xml: &str) -> Vec<RawEntry> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut entries = Vec::new();
    let mut current_element = Vec::new();
    let mut in_entry = false;
    let mut current = RawEntry::default();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = qname.as_ref();
                current_element.clear();
                current_element.extend_from_slice(name);

                if name == b"stuff" {
                    in_entry = true;
                    current = RawEntry::default();
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"stuff" && in_entry {
                    entries.push(std::mem::take(&mut current));
                    in_entry = false;
                }
            }
            Ok(Event::Text(e)) => {
                if !in_entry {
                    continue;
                }

                // Defer the UTF-8 allocation to the field that actually needs it;
                // the registry format has ~13 ignored fields per entry (providerid,
                // author, rating, summary, …) so this avoids most allocations.
                match current_element.as_slice() {
                    b"name" => current.name = String::from_utf8_lossy(e.as_ref()).into_owned(),
                    b"version" => {
                        current.version = String::from_utf8_lossy(e.as_ref()).into_owned();
                    }
                    b"id" => current.id_text = String::from_utf8_lossy(e.as_ref()).into_owned(),
                    b"releasedate" => {
                        current.release_date = String::from_utf8_lossy(e.as_ref()).into_owned();
                    }
                    b"installedfile" => {
                        current
                            .installed_files
                            .push(String::from_utf8_lossy(e.as_ref()).into_owned());
                    }
                    b"uninstalledfile" => {
                        current
                            .uninstalled_files
                            .push(String::from_utf8_lossy(e.as_ref()).into_owned());
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    entries
}

/// Parses all entries from a KNewStuff registry file.
pub(super) fn parse_registry_entries(xml: &str) -> Vec<RegistryEntry> {
    parse_raw_entries(xml)
        .into_iter()
        .filter_map(|raw| {
            let installed_path = raw.first_installed_path()?;
            if raw.name.is_empty() || installed_path.as_os_str().is_empty() {
                return None;
            }
            Some(RegistryEntry {
                name: raw.name,
                version: raw.version,
                installed_path,
                release_date: raw.release_date,
            })
        })
        .collect()
}

/// Creates an empty registry file with the proper XML structure.
pub(super) fn create_empty_registry() -> String {
    EMPTY_REGISTRY_TEMPLATE.to_string()
}

/// Escapes special characters for XML text content.
///
/// Returns a borrowed slice when no escaping is needed (the common case),
/// avoiding a heap allocation entirely.
fn escape_xml_text(s: &str) -> Cow<'_, str> {
    let Some(first) = s.find(['&', '<', '>']) else {
        return Cow::Borrowed(s);
    };
    let mut out = String::with_capacity(s.len() + 4);
    out.push_str(&s[..first]);
    for c in s[first..].chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Adds a new entry to the registry XML.
pub(super) fn add_entry(xml: &str, entry: &NewEntry) -> String {
    let category_id = entry.component_type.category_id();
    let store_url = format!("https://store.kde.org/p/{}", entry.content_id);
    let installed_file = utils::registry_installed_file_path(entry.installed_path);

    let new_entry = format!(
        r#"  <stuff category="{category_id}">
    <name>{name}</name>
    <providerid>api.kde-look.org</providerid>
    <author></author>
    <homepage>{store_url}</homepage>
    <licence></licence>
    <version>{version}</version>
    <rating>0</rating>
    <downloads>0</downloads>
    <installedfile>{installed_file}</installedfile>
    <id>{content_id}</id>
    <releasedate>{release_date}</releasedate>
    <summary></summary>
    <changelog></changelog>
    <preview></preview>
    <previewBig></previewBig>
    <payload>{download_url}</payload>
    <tags></tags>
    <status>installed</status>
  </stuff>
"#,
        name = escape_xml_text(entry.name),
        version = escape_xml_text(entry.version),
        download_url = escape_xml_text(entry.download_url),
        content_id = entry.content_id,
        release_date = entry.release_date,
    );

    if let Some(pos) = xml.rfind("</hotnewstuffregistry>") {
        let suffix = "</hotnewstuffregistry>\n";
        let mut result = String::with_capacity(pos + new_entry.len() + suffix.len());
        result.push_str(&xml[..pos]);
        result.push_str(&new_entry);
        result.push_str(suffix);
        result
    } else {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
{new_entry}</hotnewstuffregistry>
"#
        )
    }
}

/// Updates an existing entry in the registry XML.
/// Returns `Some(new_xml)` if entry was found and updated, `None` if not found.
pub(super) fn update_entry(xml: &str, fields: &UpdateFields) -> Result<Option<String>> {
    let Some(target_index) = find_target_index(xml, fields.directory_name) else {
        return Ok(None);
    };

    rewrite_with_updates(xml, target_index, fields)
}

/// Returns the 0-based index of the `<stuff>` entry whose installed or uninstalled
/// file paths contain `directory_name` as a path segment.
fn find_target_index(xml: &str, directory_name: &str) -> Option<usize> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let dir_bytes = directory_name.as_bytes();

    let mut current_element: Vec<u8> = Vec::new();
    let mut entry_index: Option<usize> = None;
    let mut in_entry = false;
    let mut current_matches = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = qname.as_ref();
                current_element.clear();
                current_element.extend_from_slice(name);

                if name == b"stuff" {
                    in_entry = true;
                    entry_index = Some(entry_index.map_or(0, |i| i + 1));
                    current_matches = false;
                }
            }
            Ok(Event::Text(e)) if in_entry => {
                if !current_matches
                    && matches!(
                        current_element.as_slice(),
                        b"installedfile" | b"uninstalledfile"
                    )
                {
                    // Check path segments at byte level — no UTF-8 conversion needed.
                    current_matches = (*e).split(|&b| b == b'/').any(|seg| seg == dir_bytes);
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"stuff" && in_entry {
                    if current_matches {
                        return entry_index;
                    }
                    in_entry = false;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    None
}

/// Rewrites the registry XML, updating fields in the target entry.
fn rewrite_with_updates(
    xml: &str,
    target_index: usize,
    fields: &UpdateFields,
) -> Result<Option<String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut writer = Writer::new(Vec::new());
    let mut current_element = Vec::new();
    let mut entry_index: Option<usize> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let name = qname.as_ref();
                current_element.clear();
                current_element.extend_from_slice(name);

                if name == b"stuff" {
                    entry_index = Some(entry_index.map_or(0, |i| i + 1));
                }

                writer.write_event(Event::Start(e.clone()))?;
            }
            Ok(Event::End(e)) => {
                writer.write_event(Event::End(e))?;
            }
            Ok(Event::Text(e)) => {
                let in_target = entry_index == Some(target_index);

                if in_target
                    && let Some(replacement) = get_field_replacement(&current_element, fields)
                {
                    writer.write_event(Event::Text(quick_xml::events::BytesText::new(
                        &replacement,
                    )))?;
                    continue;
                }

                writer.write_event(Event::Text(e))?;
            }
            Ok(Event::Eof) => break,
            Ok(e) => {
                writer.write_event(e)?;
            }
            Err(e) => {
                return Err(Error::xml_parse(format!("registry xml parse error: {e}")));
            }
        }
    }

    let result = String::from_utf8(writer.into_inner())
        .map_err(|e| Error::xml_parse(format!("invalid utf8 in registry: {e}")))?;
    Ok(Some(result))
}

/// Returns the replacement value for a field being updated, or None if no replacement.
///
/// Returns borrowed slices where possible to avoid allocation for fields that
/// are already owned by the caller (`version`, `payload`, `releasedate`, `status`).
/// Only `id` (integer formatting) and `installedfile` (path formatting) allocate.
fn get_field_replacement<'a>(
    element_name: &[u8],
    fields: &'a UpdateFields,
) -> Option<Cow<'a, str>> {
    match element_name {
        b"version" => Some(Cow::Borrowed(fields.new_version)),
        b"id" => Some(Cow::Owned(fields.content_id.to_string())),
        b"payload" => Some(Cow::Borrowed(fields.download_url)),
        b"releasedate" => Some(Cow::Borrowed(fields.release_date)),
        b"status" => Some(Cow::Borrowed("installed")),
        b"installedfile" | b"uninstalledfile" => Some(Cow::Owned(
            utils::registry_installed_file_path(fields.installed_path),
        )),
        _ => None,
    }
}
