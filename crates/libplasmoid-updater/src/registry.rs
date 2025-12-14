// SPDX-License-Identifier: MIT OR Apache-2.0
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) - GPL-2.0+/LGPL-2.0+

use std::{
    fs,
    path::{Path, PathBuf},
};

use quick_xml::{Reader, Writer, events::Event};

use crate::{AvailableUpdate, ComponentType, Error, InstalledComponent, Result};

/// returns the path to the knewstuff3 directory.
pub fn knewstuff_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("knewstuff3")
}

/// entry from a knsregistry file.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub name: String,
    pub version: String,
    pub content_id: u64,
    pub installed_path: PathBuf,
    pub release_date: String,
}

/// scans registry files to discover installed components.
/// used for types that don't have metadata files (icons, wallpapers, color schemes).
pub fn scan_registry_components(component_type: ComponentType) -> Result<Vec<InstalledComponent>> {
    let Some(reg_path) = registry_path(component_type) else {
        return Ok(Vec::new());
    };

    if !reg_path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&reg_path)?;
    let entries = parse_registry_entries(&content);

    let components = entries
        .into_iter()
        .filter_map(|entry| {
            let directory_name = extract_directory_name(&entry.installed_path)?;
            Some(InstalledComponent {
                name: entry.name,
                directory_name,
                version: entry.version,
                component_type,
                path: entry.installed_path,
                is_system: false,
                release_date: entry.release_date,
            })
        })
        .collect();

    Ok(components)
}

/// parses all entries from a knsregistry file.
fn parse_registry_entries(xml: &str) -> Vec<RegistryEntry> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut entries = Vec::new();
    let mut current_element = String::new();
    let mut in_entry = false;

    let mut name = String::new();
    let mut version = String::new();
    let mut content_id = 0u64;
    let mut installed_path = PathBuf::new();
    let mut release_date = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = tag.clone();

                if tag == "stuff" {
                    in_entry = true;
                    name.clear();
                    version.clear();
                    content_id = 0;
                    installed_path = PathBuf::new();
                    release_date.clear();
                }
            }
            Ok(Event::End(e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "stuff" && in_entry {
                    if !name.is_empty() && !installed_path.as_os_str().is_empty() {
                        entries.push(RegistryEntry {
                            name: name.clone(),
                            version: version.clone(),
                            content_id,
                            installed_path: installed_path.clone(),
                            release_date: release_date.clone(),
                        });
                    }
                    in_entry = false;
                }
            }
            Ok(Event::Text(e)) => {
                if !in_entry {
                    continue;
                }

                let text = String::from_utf8_lossy(e.as_ref()).to_string();
                match current_element.as_str() {
                    "name" => name = text,
                    "version" => version = text,
                    "id" => content_id = text.parse().unwrap_or(0),
                    "releasedate" => release_date = text,
                    "installedfile" => {
                        if installed_path.as_os_str().is_empty() {
                            installed_path = PathBuf::from(text.trim_end_matches("/*"));
                        }
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

/// extracts the component directory or file name from an installed path.
/// for paths ending with metadata.json: returns parent directory name
/// for other files or directories: returns the last path component
fn extract_directory_name(path: &Path) -> Option<String> {
    let name = path.file_name().and_then(|n| n.to_str())?;

    // if path ends with metadata file, get parent directory name
    if name == "metadata.json" || name == "metadata.desktop" {
        return path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
    }

    // for everything else (single files, directories), use the last component
    Some(name.to_string())
}

/// loads registry entries into a map keyed by directory name.
/// used to look up release dates for installed components.
pub fn load_registry_map(
    component_type: ComponentType,
) -> std::collections::HashMap<String, RegistryEntry> {
    let Some(reg_path) = registry_path(component_type) else {
        return std::collections::HashMap::new();
    };

    if !reg_path.exists() {
        return std::collections::HashMap::new();
    }

    let Ok(content) = fs::read_to_string(&reg_path) else {
        return std::collections::HashMap::new();
    };

    let entries = parse_registry_entries(&content);
    entries
        .into_iter()
        .filter_map(|e| {
            let dir_name = extract_directory_name(&e.installed_path)?;
            Some((dir_name, e))
        })
        .collect()
}

/// returns the path to a specific registry file.
pub fn registry_path(component_type: ComponentType) -> Option<PathBuf> {
    component_type
        .registry_file()
        .map(|f| knewstuff_dir().join(f))
}

/// updates the kns registry after a successful component update.
/// this ensures discover sees the correct installed version.
/// if the entry doesn't exist, it creates a new one (discover-compatible).
pub fn update_registry_after_install(update: &AvailableUpdate) -> Result<()> {
    let component = &update.installed;

    let Some(reg_path) = registry_path(component.component_type) else {
        log::debug!(
            "**registry:** no registry file for {}",
            component.component_type
        );
        return Ok(());
    };

    // extract just the date part from ISO timestamp (2024-03-07T09:39:52+00:00 -> 2024-03-07)
    let release_date = extract_date_from_iso(&update.release_date);

    // ensure parent directory exists
    if let Some(parent) = reg_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let content = if reg_path.exists() {
        fs::read_to_string(&reg_path)?
    } else {
        create_empty_registry()
    };

    let updated = update_entry_in_registry(
        &content,
        &component.directory_name,
        update.content_id,
        &update.latest_version,
        &update.download_url,
        &component.path,
        &release_date,
    )?;

    if let Some(new_content) = updated {
        fs::write(&reg_path, new_content)?;
        log::debug!(
            "**registry:** updated {} for {}",
            reg_path.display(),
            component.name
        );
    } else {
        // entry not found - add new entry
        let new_content = add_entry_to_registry(
            &content,
            &component.name,
            component.component_type,
            update.content_id,
            &update.latest_version,
            &update.download_url,
            &component.path,
            &release_date,
        );
        fs::write(&reg_path, new_content)?;
        log::debug!(
            "**registry:** added {} to {}",
            component.name,
            reg_path.display()
        );
    }

    Ok(())
}

/// extracts date part from ISO timestamp (YYYY-MM-DDTHH:MM:SS+00:00 -> YYYY-MM-DD)
fn extract_date_from_iso(iso: &str) -> String {
    iso.split('T').next().unwrap_or(iso).to_string()
}

/// creates an empty registry file with the proper XML structure.
fn create_empty_registry() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
</hotnewstuffregistry>
"#
    .to_string()
}

/// escapes special characters for xml text content.
fn escape_xml_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// adds a new entry to the registry xml.
#[allow(clippy::too_many_arguments)]
fn add_entry_to_registry(
    xml: &str,
    name: &str,
    component_type: ComponentType,
    content_id: u64,
    version: &str,
    download_url: &str,
    installed_path: &std::path::Path,
    release_date: &str,
) -> String {
    let category_id = component_type.category_id();
    let store_url = format!("https://store.kde.org/p/{content_id}");
    let installed_file = format!("{}/metadata.json", installed_path.to_string_lossy());

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
        name = escape_xml_text(name),
        version = escape_xml_text(version),
        download_url = escape_xml_text(download_url),
    );

    // insert before </hotnewstuffregistry>
    if let Some(pos) = xml.rfind("</hotnewstuffregistry>") {
        let mut result = xml[..pos].to_string();
        result.push_str(&new_entry);
        result.push_str("</hotnewstuffregistry>\n");
        result
    } else {
        // malformed xml, create new registry with entry
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
{new_entry}</hotnewstuffregistry>
"#
        )
    }
}

/// updates an existing entry in the registry xml.
/// returns Some(new_xml) if entry was found and updated, None if not found.
fn update_entry_in_registry(
    xml: &str,
    directory_name: &str,
    content_id: u64,
    new_version: &str,
    download_url: &str,
    installed_path: &Path,
    release_date: &str,
) -> Result<Option<String>> {
    // first pass: find which entry index contains our target
    let target_index = find_target_entry_index(xml, directory_name);

    let Some(target_idx) = target_index else {
        return Ok(None);
    };

    // second pass: rewrite xml, updating fields when in target entry
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut writer = Writer::new(Vec::new());
    let mut current_element = String::new();
    let mut entry_index: i32 = -1;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = name.clone();

                if name == "stuff" {
                    entry_index += 1;
                }

                writer.write_event(Event::Start(e.clone()))?;
            }
            Ok(Event::End(e)) => {
                writer.write_event(Event::End(e))?;
            }
            Ok(Event::Text(e)) => {
                // check if we're in the target entry
                let in_target = entry_index == target_idx as i32;

                if in_target {
                    match current_element.as_str() {
                        "version" => {
                            writer.write_event(Event::Text(quick_xml::events::BytesText::new(
                                new_version,
                            )))?;
                            continue;
                        }
                        "id" => {
                            writer.write_event(Event::Text(quick_xml::events::BytesText::new(
                                &content_id.to_string(),
                            )))?;
                            continue;
                        }
                        "payload" => {
                            writer.write_event(Event::Text(quick_xml::events::BytesText::new(
                                download_url,
                            )))?;
                            continue;
                        }
                        "releasedate" => {
                            writer.write_event(Event::Text(quick_xml::events::BytesText::new(
                                release_date,
                            )))?;
                            continue;
                        }
                        "status" => {
                            writer.write_event(Event::Text(quick_xml::events::BytesText::new(
                                "installed",
                            )))?;
                            continue;
                        }
                        "installedfile" | "uninstalledfile" => {
                            let new_path =
                                format!("{}/metadata.json", installed_path.to_string_lossy());
                            writer.write_event(Event::Text(quick_xml::events::BytesText::new(
                                &new_path,
                            )))?;
                            continue;
                        }
                        _ => {}
                    }
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

/// find the index of the entry containing the target directory
fn find_target_entry_index(xml: &str, directory_name: &str) -> Option<usize> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut current_element = String::new();
    let mut entry_index: i32 = -1;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                current_element = name.clone();

                if name == "stuff" {
                    entry_index += 1;
                }
            }
            Ok(Event::Text(e)) => {
                if current_element == "installedfile" || current_element == "uninstalledfile" {
                    let text = String::from_utf8_lossy(e.as_ref()).to_string();
                    if path_matches_directory(&text, directory_name) {
                        return Some(entry_index as usize);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    None
}

fn path_matches_directory(path: &str, directory_name: &str) -> bool {
    path.split('/').any(|segment| segment == directory_name)
}
