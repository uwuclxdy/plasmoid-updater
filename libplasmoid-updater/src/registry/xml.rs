// SPDX-License-Identifier: GPL-3.0-or-later
//
// KNewStuff registry format based on KDE Discover (https://invent.kde.org/plasma/discover) -
// GPL-2.0-only OR GPL-3.0-only OR LicenseRef-KDE-Accepted-GPL

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use quick_xml::{
    Reader, Writer,
    escape::resolve_predefined_entity,
    events::{BytesEnd, BytesText, Event},
};

use crate::{
    types::ComponentType,
    {Error, Result},
};

use super::{manager::RegistryEntry, utils};

const CLOSING_TAG: &str = "</hotnewstuffregistry>";

const REGISTRY_HEADER: &str =
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE khotnewstuff3>\n<hotnewstuffregistry>\n";

/// Returns a whole registry document holding `entries` (already serialized).
fn registry_document(entries: &str) -> String {
    format!("{REGISTRY_HEADER}{entries}{CLOSING_TAG}\n")
}

/// Returns whether `xml` holds nothing but a byte-order mark and whitespace: a
/// fresh registry with no entry to lose.
fn is_blank(xml: &str) -> bool {
    xml.trim_start_matches('\u{feff}').trim().is_empty()
}

/// An element's text without the surrounding whitespace a pretty-printed
/// document adds, the spelling both the reader and the writer compare.
fn element_text(raw: &str) -> &str {
    raw.trim()
}

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

    /// Records one finished element's text into the field it names.
    fn assign(&mut self, element: &str, raw: &str) {
        let value = element_text(raw);
        match element {
            "name" => self.name = value.to_string(),
            "version" => self.version = value.to_string(),
            "id" => self.id_text = value.to_string(),
            "releasedate" => self.release_date = value.to_string(),
            "installedfile" if !value.is_empty() => self.installed_files.push(value.to_string()),
            "uninstalledfile" if !value.is_empty() => {
                self.uninstalled_files.push(value.to_string());
            }
            _ => {}
        }
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
    pub component_type: ComponentType,
    pub content_id: u64,
    pub new_version: &'a str,
    pub download_url: &'a str,
    pub installed_path: &'a Path,
    pub release_date: &'a str,
    /// Whether a unique directory-name match may select the entry when no
    /// `installedfile` names the install path: true for a user write, false for
    /// a `--system` write, which must never pick an entry by name.
    pub name_fallback: bool,
}

fn parse_error(detail: impl Into<String>) -> Error {
    Error::Parse {
        what: "knewstuff registry",
        detail: detail.into(),
    }
}

/// Decodes one reference fragment (`&amp;`, `&#233;`, `&#xE9;`) into `text`.
fn decode_ref(text: &mut String, reference: &quick_xml::events::BytesRef<'_>) -> Result<()> {
    let name = reference.as_ref();
    let character = reference
        .resolve_char_ref()
        .map_err(|e| parse_error(format!("invalid character reference &{name};: {e}")))?;
    if let Some(character) = character {
        text.push(character);
        return Ok(());
    }
    let Some(expansion) = resolve_predefined_entity(name) else {
        return Err(parse_error(format!("unresolved entity reference &{name};")));
    };
    text.push_str(expansion);
    Ok(())
}

/// Parses all `<stuff>` entries from registry XML into raw field collections.
///
/// Every element's text is collected from all the fragments quick-xml reports
/// for it — `Event::Text`, `Event::CData` and `Event::GeneralRef` — and decoded
/// once at its end tag, so `A &amp; B` reads as `A & B` instead of as its last
/// fragment.
///
/// Content holding only a byte-order mark and whitespace is an empty registry;
/// any other content without a `hotnewstuffregistry` root element is an error,
/// never zero entries.
pub(super) fn parse_raw_entries(xml: &str) -> Result<Vec<RawEntry>> {
    let mut reader = Reader::from_str(xml);

    let mut entries = Vec::new();
    let mut current_element = String::new();
    let mut text = String::new();
    let mut in_entry = false;
    let mut current = RawEntry::default();
    let mut open = 0usize;
    let mut saw_root = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                open += 1;
                current_element.clear();
                current_element.push_str(e.name().as_ref());
                text.clear();

                if current_element == "hotnewstuffregistry" {
                    saw_root = true;
                } else if current_element == "stuff" {
                    in_entry = true;
                    current = RawEntry::default();
                }
            }
            Ok(Event::Empty(e)) if e.name().as_ref() == "hotnewstuffregistry" => saw_root = true,
            Ok(Event::Text(e)) if in_entry => text.push_str(&e),
            Ok(Event::CData(e)) if in_entry => text.push_str(&e),
            Ok(Event::GeneralRef(e)) if in_entry => decode_ref(&mut text, &e)?,
            Ok(Event::End(e)) => {
                open = open.saturating_sub(1);
                let name = e.name();
                if in_entry {
                    current.assign(name.as_ref(), &text);
                    if name.as_ref() == "stuff" {
                        entries.push(std::mem::take(&mut current));
                        in_entry = false;
                    }
                }
                text.clear();
            }
            Ok(Event::Eof) => {
                if open != 0 {
                    return Err(parse_error(format!(
                        "unexpected end of file inside {open} unclosed element(s)"
                    )));
                }
                if !saw_root && !is_blank(xml) {
                    return Err(parse_error("missing <hotnewstuffregistry> root element"));
                }
                break;
            }
            Err(e) => return Err(parse_error(e.to_string())),
            _ => {}
        }
    }

    Ok(entries)
}

/// Parses all entries from a KNewStuff registry file.
pub(super) fn parse_registry_entries(xml: &str) -> Result<Vec<RegistryEntry>> {
    Ok(parse_raw_entries(xml)?
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
        .collect())
}

/// Creates an empty registry file with the proper XML structure.
pub(super) fn create_empty_registry() -> String {
    registry_document("")
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
///
/// Every interpolated value is escaped, so a path or date carrying `&`, `<` or
/// `>` cannot break the document. Blank content and a self-closing root are
/// written as a fresh registry, since neither holds an entry to lose. A
/// document without its closing tag is an error: rebuilding it would drop the
/// entries a truncated registry still holds.
pub(super) fn add_entry(xml: &str, entry: &NewEntry) -> Result<String> {
    let category_id = entry.component_type.category_id();
    let store_url = crate::types::store_page_url(entry.content_id);
    let installed_file =
        utils::registry_installed_file_path(entry.installed_path, entry.component_type);

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
        category_id = category_id,
        name = escape_xml_text(entry.name),
        store_url = escape_xml_text(&store_url),
        version = escape_xml_text(entry.version),
        installed_file = escape_xml_text(&installed_file),
        content_id = entry.content_id,
        release_date = escape_xml_text(entry.release_date),
        download_url = escape_xml_text(entry.download_url),
    );

    if is_blank(xml) {
        return Ok(registry_document(&new_entry));
    }
    let insert_at = match root_end(xml)? {
        RootEnd::SelfClosing => return Ok(registry_document(&new_entry)),
        RootEnd::ClosingTag(offset) => offset,
    };
    let mut result = String::with_capacity(xml.len() + new_entry.len());
    result.push_str(&xml[..insert_at]);
    result.push_str(&new_entry);
    result.push_str(&xml[insert_at..]);
    Ok(result)
}

/// How a registry document's root element ends.
enum RootEnd {
    /// `<hotnewstuffregistry/>`: a valid registry holding no entry.
    SelfClosing,
    /// The byte offset of `</hotnewstuffregistry>`.
    ClosingTag(usize),
}

/// Returns how the root element ends.
///
/// The parser finds the closing tag, so the literal `</hotnewstuffregistry>`
/// inside a comment, a CDATA section or text can never be mistaken for the
/// root's end.
fn root_end(xml: &str) -> Result<RootEnd> {
    let mut reader = Reader::from_str(xml);
    let mut saw_root = false;

    loop {
        let before = reader.buffer_position() as usize;
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == "hotnewstuffregistry" {
                    saw_root = true;
                }
            }
            Ok(Event::Empty(e)) => {
                if e.name().as_ref() == "hotnewstuffregistry" {
                    return Ok(RootEnd::SelfClosing);
                }
            }
            Ok(Event::End(e)) => {
                if saw_root && e.name().as_ref() == "hotnewstuffregistry" {
                    return Ok(RootEnd::ClosingTag(before));
                }
            }
            Ok(Event::Eof) => {
                if !saw_root {
                    return Err(parse_error("missing <hotnewstuffregistry> root element"));
                }
                return Err(parse_error(format!("missing {CLOSING_TAG} closing tag")));
            }
            Err(e) => return Err(parse_error(e.to_string())),
            _ => {}
        }
    }
}

/// Updates an existing entry in the registry XML.
/// Returns `Some(new_xml)` if entry was found and updated, `None` if not found.
pub(super) fn update_entry(xml: &str, fields: &UpdateFields) -> Result<Option<String>> {
    let Some(target) = find_target_index(
        xml,
        fields.installed_path,
        fields.directory_name,
        fields.name_fallback,
    )?
    else {
        return Ok(None);
    };

    rewrite_with_updates(xml, target, fields)
}

/// Locates the `<stuff>` entry an update belongs to, and which of that entry's
/// `installedfile` elements matched.
///
/// An entry whose `installedfile` names the normalized resolved install root,
/// the spelling the id cache keys an install by, wins, so an entry naming
/// another root's same-named install never answers. When none does and
/// `name_fallback` is set (a user write), the one entry of this registry whose
/// `installedfile` names `directory_name` as a path segment answers instead:
/// that repairs a record made under a spelling normalization cannot fold, such
/// as a moved or symlinked data directory. Two such entries are ambiguous, and
/// appending a record beats guessing which one is stale. A `--system` write
/// never falls back. `uninstalledfile` records files a component removed, so it
/// never names the entry an update belongs to.
fn find_target_index(
    xml: &str,
    installed_path: &Path,
    directory_name: &str,
    name_fallback: bool,
) -> Result<Option<(usize, usize)>> {
    let wanted = utils::normalize_installed_path(&installed_path.to_string_lossy());
    let entries = installed_files_by_entry(xml)?;

    let exact = entries.iter().enumerate().find_map(|(entry, files)| {
        files
            .iter()
            .position(|file| utils::normalize_installed_path(file) == wanted)
            .map(|ordinal| (entry, ordinal + 1))
    });
    if exact.is_some() || !name_fallback {
        return Ok(exact);
    }

    let mut named = entries.iter().enumerate().filter_map(|(entry, files)| {
        files
            .iter()
            .position(|file| file.split('/').any(|segment| segment == directory_name))
            .map(|ordinal| (entry, ordinal + 1))
    });
    let Some(first) = named.next() else {
        return Ok(None);
    };
    if named.next().is_some() {
        return Ok(None);
    }
    Ok(Some(first))
}

/// Every `<stuff>` entry's `installedfile` values, in document order. An
/// entry's file ordinal is its index here plus one, empty elements included,
/// which is how the rewrite counts them.
fn installed_files_by_entry(xml: &str) -> Result<Vec<Vec<String>>> {
    let mut reader = Reader::from_str(xml);

    let mut current_element = String::new();
    let mut text = String::new();
    let mut entries: Vec<Vec<String>> = Vec::new();
    let mut in_entry = false;
    let mut open = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                open += 1;
                current_element.clear();
                current_element.push_str(e.name().as_ref());
                text.clear();

                if current_element == "stuff" {
                    in_entry = true;
                    entries.push(Vec::new());
                }
            }
            Ok(Event::Empty(e)) if in_entry && e.name().as_ref() == "installedfile" => {
                if let Some(files) = entries.last_mut() {
                    files.push(String::new());
                }
            }
            Ok(Event::Text(e)) if in_entry && current_element == "installedfile" => {
                text.push_str(&e);
            }
            Ok(Event::CData(e)) if in_entry && current_element == "installedfile" => {
                text.push_str(&e);
            }
            Ok(Event::GeneralRef(e)) if in_entry && current_element == "installedfile" => {
                decode_ref(&mut text, &e)?;
            }
            Ok(Event::End(e)) => {
                open = open.saturating_sub(1);
                let name = e.name();
                if in_entry
                    && name.as_ref() == "installedfile"
                    && let Some(files) = entries.last_mut()
                {
                    files.push(element_text(&text).to_string());
                }
                if in_entry && name.as_ref() == "stuff" {
                    in_entry = false;
                }
                text.clear();
            }
            Ok(Event::Eof) => {
                if open != 0 {
                    return Err(parse_error(format!(
                        "unexpected end of file inside {open} unclosed element(s)"
                    )));
                }
                break;
            }
            Err(e) => return Err(parse_error(e.to_string())),
            _ => {}
        }
    }

    Ok(entries)
}

/// Rewrites the registry XML, updating fields in the target entry.
///
/// Every other event is written back exactly as read, so foreign entries stay
/// byte-for-byte identical. Only elements inside the target `<stuff>` are
/// candidates: an element at root level or inside another entry is never
/// rewritten. A replaced element's whole content is replaced — whatever
/// fragment count or nested markup its old value had, and whether it was
/// self-closing — and only the `installedfile` that matched is rewritten;
/// `uninstalledfile` never is.
fn rewrite_with_updates<'a>(
    xml: &str,
    target: (usize, usize),
    fields: &'a UpdateFields,
) -> Result<Option<String>> {
    let (target_entry, target_installed_file) = target;
    let mut reader = Reader::from_str(xml);
    let mut writer = Writer::new(Vec::new());

    let mut current_element = String::new();
    let mut entry_index: Option<usize> = None;
    // the depth of the target `<stuff>` while the reader is inside it
    let mut target_depth: Option<usize> = None;
    let mut installed_ordinal = 0usize;
    let mut depth = 0usize;
    let mut replacing_at: Option<usize> = None;
    let mut replacement: Option<Cow<'a, str>> = None;
    let mut closed = false;

    // a fn, not a closure: Event borrows from the caller's locals, and a
    // closure would pin one lifetime across all call sites
    fn write(writer: &mut Writer<Vec<u8>>, event: Event<'_>) -> Result<()> {
        writer
            .write_event(event)
            .map_err(|e| parse_error(format!("serialization failed: {e}")))
    }

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                depth += 1;
                if replacing_at.is_some() {
                    continue;
                }
                current_element.clear();
                current_element.push_str(e.name().as_ref());
                if current_element == "stuff" {
                    entry_index = Some(entry_index.map_or(0, |index| index + 1));
                    installed_ordinal = 0;
                    if entry_index == Some(target_entry) {
                        target_depth = Some(depth);
                    }
                } else if target_depth.is_some() {
                    let replace = if current_element == "installedfile" {
                        installed_ordinal += 1;
                        installed_ordinal == target_installed_file
                    } else {
                        get_field_replacement(&current_element, fields).is_some()
                    };
                    if replace {
                        replacement = get_field_replacement(&current_element, fields);
                        replacing_at = Some(depth);
                    }
                }
                write(&mut writer, Event::Start(e))?;
            }
            Ok(Event::Empty(e)) => {
                if replacing_at.is_some() {
                    continue;
                }
                current_element.clear();
                current_element.push_str(e.name().as_ref());
                let in_target = target_depth.is_some() && current_element != "stuff";
                let replace = in_target
                    && if current_element == "installedfile" {
                        installed_ordinal += 1;
                        installed_ordinal == target_installed_file
                    } else {
                        get_field_replacement(&current_element, fields).is_some()
                    };
                if !replace {
                    write(&mut writer, Event::Empty(e))?;
                    continue;
                }
                let value = get_field_replacement(&current_element, fields);
                let end = BytesEnd::new(current_element.clone());
                write(&mut writer, Event::Start(e))?;
                if let Some(value) = value {
                    write(&mut writer, Event::Text(BytesText::new(&value)))?;
                }
                write(&mut writer, Event::End(end))?;
            }
            Ok(Event::Text(e)) => {
                if replacing_at.is_none() {
                    write(&mut writer, Event::Text(e))?;
                }
            }
            Ok(Event::CData(e)) => {
                if replacing_at.is_none() {
                    write(&mut writer, Event::CData(e))?;
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if replacing_at.is_none() {
                    write(&mut writer, Event::GeneralRef(e))?;
                }
            }
            Ok(Event::End(e)) => {
                let is_root = e.name().as_ref() == "hotnewstuffregistry";
                let closes_target = target_depth == Some(depth);
                let closes_replacement = replacing_at == Some(depth);
                depth = depth.saturating_sub(1);
                if closes_replacement {
                    if let Some(value) = replacement.take() {
                        write(&mut writer, Event::Text(BytesText::new(&value)))?;
                    }
                    replacing_at = None;
                }
                if replacing_at.is_some() {
                    continue;
                }
                if closes_target {
                    target_depth = None;
                }
                if is_root {
                    closed = true;
                }
                write(&mut writer, Event::End(e))?;
            }
            Ok(Event::Eof) => break,
            Ok(event) => {
                if replacing_at.is_none() {
                    write(&mut writer, event)?;
                }
            }
            Err(e) => return Err(parse_error(e.to_string())),
        }
    }

    if !closed {
        return Err(parse_error(format!("missing {CLOSING_TAG} closing tag")));
    }

    let result = String::from_utf8(writer.into_inner())
        .map_err(|e| parse_error(format!("invalid utf8: {e}")))?;
    Ok(Some(result))
}

/// Returns the replacement value for a field being updated, or None if no replacement.
///
/// Returns borrowed slices where possible to avoid allocation for fields that
/// are already owned by the caller (`version`, `payload`, `releasedate`, `status`).
/// Only `id` (integer formatting) and `installedfile` (path formatting) allocate.
fn get_field_replacement<'a>(element_name: &str, fields: &'a UpdateFields) -> Option<Cow<'a, str>> {
    match element_name {
        "version" => Some(Cow::Borrowed(fields.new_version)),
        "id" => Some(Cow::Owned(fields.content_id.to_string())),
        "payload" => Some(Cow::Borrowed(fields.download_url)),
        "releasedate" => Some(Cow::Borrowed(fields.release_date)),
        "status" => Some(Cow::Borrowed("installed")),
        "installedfile" => Some(Cow::Owned(utils::registry_installed_file_path(
            fields.installed_path,
            fields.component_type,
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_entry(installed_path: &Path) -> NewEntry<'_> {
        NewEntry {
            name: "Fake & Caf<é>",
            component_type: ComponentType::ColorScheme,
            content_id: 7,
            version: "1.0 & beta",
            download_url: "https://example.test/dl?a=1&b=2",
            installed_path,
            release_date: "2026-01-01 & later",
        }
    }

    fn update_fields<'a>(directory_name: &'a str, installed_path: &'a Path) -> UpdateFields<'a> {
        UpdateFields {
            directory_name,
            component_type: ComponentType::ColorScheme,
            content_id: 777,
            new_version: "2.0",
            download_url: "https://example.test/dl?new=1",
            installed_path,
            release_date: "2026-02-02",
            name_fallback: true,
        }
    }

    #[test]
    fn entities_decode_and_fragments_append_in_every_read_field() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Fake &amp; Caf&#233; &lt;lit&gt; &quot;q&quot; &#xE9;</name>
    <version>1.0 &amp; beta</version>
    <releasedate>2026-01-01&amp;later</releasedate>
    <installedfile>/home/u/my&amp;dir/f/metadata.json</installedfile>
    <uninstalledfile>/home/u/gone&amp;old/f</uninstalledfile>
    <id>999001</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let entries = parse_raw_entries(xml).unwrap();

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.name, "Fake & Café <lit> \"q\" é");
        assert_eq!(entry.version, "1.0 & beta");
        assert_eq!(entry.release_date, "2026-01-01&later");
        assert_eq!(
            entry.installed_files,
            vec!["/home/u/my&dir/f/metadata.json"]
        );
        assert_eq!(entry.uninstalled_files, vec!["/home/u/gone&old/f"]);
        assert_eq!(entry.id_text, "999001");
    }

    #[test]
    fn add_entry_escapes_every_interpolated_value() {
        let entry = new_entry(Path::new("/tmp/a&b<scheme>"));

        let out = add_entry(&create_empty_registry(), &entry).unwrap();

        assert!(
            out.contains("/tmp/a&amp;b&lt;scheme&gt;/metadata.json"),
            "{out}"
        );
        assert!(
            out.contains("<releasedate>2026-01-01 &amp; later</releasedate>"),
            "{out}"
        );
        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed[0].name, "Fake & Caf<é>");
        assert_eq!(parsed[0].version, "1.0 & beta");
        assert_eq!(parsed[0].release_date, "2026-01-01 & later");
        assert_eq!(
            parsed[0].installed_files,
            vec!["/tmp/a&b<scheme>/metadata.json"]
        );
    }

    #[test]
    fn add_entry_writes_the_store_page_homepage() {
        let entry = new_entry(Path::new("/tmp/scheme"));

        let out = add_entry(&create_empty_registry(), &entry).unwrap();

        let (_, rest) = out.split_once("<homepage>").unwrap();
        let (homepage, _) = rest.split_once("</homepage>").unwrap();
        assert_eq!(homepage, "https://store.kde.org/p/7");
    }

    #[test]
    fn add_entry_refuses_a_registry_without_its_closing_tag() {
        let truncated = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE khotnewstuff3>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Foreign</name>
    <installedfile>/home/u/foreign/metadata.json</installedfile>
    <id>1</id>
  </stuff>"#;

        let err = add_entry(truncated, &new_entry(Path::new("/tmp/a"))).unwrap_err();

        assert_eq!(
            err.to_string(),
            "failed to parse knewstuff registry: missing </hotnewstuffregistry> closing tag"
        );
    }

    #[test]
    fn update_matches_installedfile_never_uninstalledfile() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Other</name>
    <version>1.0</version>
    <installedfile>/home/u/other/Other.colors</installedfile>
    <uninstalledfile>/home/u/target/Target.colors</uninstalledfile>
    <id>111</id>
  </stuff>
  <stuff category="112">
    <name>Target</name>
    <version>1.0</version>
    <installedfile>/home/u/target/Target.colors</installedfile>
    <id>222</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let out = update_entry(
            xml,
            &update_fields("Target.colors", Path::new("/home/u/target/Target.colors")),
        )
        .unwrap()
        .unwrap();

        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id_text, "111");
        assert_eq!(parsed[0].version, "1.0");
        assert_eq!(
            parsed[0].uninstalled_files,
            vec!["/home/u/target/Target.colors"]
        );
        assert_eq!(parsed[1].id_text, "777");
        assert_eq!(parsed[1].version, "2.0");
    }

    /// A registry holding two same-dir entries under different roots: the
    /// update rewrites only the entry whose `installedfile` names the
    /// component's own install path.
    #[test]
    fn update_targets_the_entry_whose_installedfile_names_the_install_path() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Fake</name>
    <version>1.0</version>
    <installedfile>/usr/local/share/color-schemes/Fake.colors</installedfile>
    <id>111</id>
  </stuff>
  <stuff category="112">
    <name>Fake</name>
    <version>1.0</version>
    <installedfile>/usr/share/color-schemes/Fake.colors</installedfile>
    <id>222</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let fields = UpdateFields {
            directory_name: "Fake.colors",
            component_type: ComponentType::ColorScheme,
            content_id: 777,
            new_version: "2.0",
            download_url: "https://example.test/dl?new=1",
            installed_path: Path::new("/usr/share/color-schemes/Fake.colors"),
            release_date: "2026-02-02",
            name_fallback: true,
        };
        let out = update_entry(xml, &fields).unwrap().unwrap();

        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(
            parsed[0].id_text, "111",
            "the other root's entry was rewritten"
        );
        assert_eq!(
            parsed[1].id_text, "777",
            "the own root's entry was not rewritten"
        );
    }

    /// A user registry entry recorded under a spelling normalization cannot
    /// fold — a moved or symlinked data directory — still names the same
    /// directory: the fallback rewrites it instead of appending a duplicate.
    #[test]
    fn update_falls_back_to_the_one_entry_naming_the_directory() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Fake</name>
    <version>1.0</version>
    <installedfile>/home/u/.local/share/color-schemes/Fake.colors</installedfile>
    <id>111</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let out = update_entry(
            xml,
            &update_fields(
                "Fake.colors",
                Path::new("/data/u/share/color-schemes/Fake.colors"),
            ),
        )
        .unwrap()
        .unwrap();

        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed.len(), 1, "the update appended a duplicate:\n{out}");
        assert_eq!(parsed[0].id_text, "777");
        assert_eq!(parsed[0].version, "2.0");
        assert_eq!(
            parsed[0].installed_files,
            vec!["/data/u/share/color-schemes/Fake.colors/metadata.json"]
        );
    }

    /// A `--system` write never selects an entry by directory name: a name-only
    /// match appends a record instead of rewriting an entry it cannot identify.
    #[test]
    fn a_system_write_never_matches_an_entry_by_name() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Fake</name>
    <version>1.0</version>
    <installedfile>/home/u/.local/share/color-schemes/Fake.colors</installedfile>
    <id>111</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let mut fields = update_fields(
            "Fake.colors",
            Path::new("/data/u/share/color-schemes/Fake.colors"),
        );
        fields.name_fallback = false;

        assert_eq!(
            update_entry(xml, &fields).unwrap(),
            None,
            "a system write picked an entry by directory name"
        );
    }

    /// Two entries of this registry name one directory: no unique fallback, so
    /// the update appends instead of guessing which record is the stale one.
    #[test]
    fn update_appends_when_two_entries_name_the_directory() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Fake</name>
    <version>1.0</version>
    <installedfile>/home/u/first/color-schemes/Fake.colors</installedfile>
    <id>111</id>
  </stuff>
  <stuff category="112">
    <name>Fake</name>
    <version>1.0</version>
    <installedfile>/home/u/second/color-schemes/Fake.colors</installedfile>
    <id>222</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let fields = update_fields(
            "Fake.colors",
            Path::new("/data/u/share/color-schemes/Fake.colors"),
        );

        assert_eq!(
            update_entry(xml, &fields).unwrap(),
            None,
            "an ambiguous directory-name match picked an entry"
        );
        let appended = add_entry(
            xml,
            &new_entry(Path::new("/data/u/share/color-schemes/Fake.colors")),
        )
        .unwrap();
        assert_eq!(
            parse_raw_entries(&appended).unwrap().len(),
            3,
            "the new record was not appended"
        );
    }

    /// A pretty-printed `installedfile` carries whitespace inside the element:
    /// the write side trims it as the read side does, so it rewrites the entry
    /// instead of appending a duplicate.
    #[test]
    fn update_matches_an_installedfile_holding_surrounding_whitespace() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<hotnewstuffregistry>\n  <stuff category=\"112\">\n    <name>Target</name>\n    <version>1.0</version>\n    <installedfile>\n      /home/u/target/Target.colors\n    </installedfile>\n    <id>222</id>\n  </stuff>\n</hotnewstuffregistry>\n";

        let out = update_entry(
            xml,
            &update_fields("Target.colors", Path::new("/home/u/target/Target.colors")),
        )
        .unwrap()
        .unwrap();

        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed.len(), 1, "the update appended a duplicate:\n{out}");
        assert_eq!(parsed[0].id_text, "777");
        assert_eq!(parsed[0].version, "2.0");
    }

    #[test]
    fn update_replaces_the_installedfile_that_matched() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Target</name>
    <version>1.0</version>
    <installedfile>/home/u/target/Target.png</installedfile>
    <installedfile>/home/u/target/Target.colors</installedfile>
    <uninstalledfile>/home/u/gone/Old.colors</uninstalledfile>
    <id>222</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let out = update_entry(
            xml,
            &update_fields("Target.colors", Path::new("/home/u/target/replacement")),
        )
        .unwrap()
        .unwrap();

        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(
            parsed[0].installed_files,
            vec![
                "/home/u/target/Target.png",
                "/home/u/target/replacement/metadata.json"
            ]
        );
        assert_eq!(parsed[0].uninstalled_files, vec!["/home/u/gone/Old.colors"]);
    }

    #[test]
    fn update_keeps_untouched_entries_byte_for_byte() {
        let foreign = "  <stuff category=\"112\">\n    <name>Other &amp; More</name>\n    <installedfile>/home/u/other/Other.colors</installedfile>\n    <id>111</id>\n  </stuff>\n";
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE khotnewstuff3>\n<hotnewstuffregistry>\n{foreign}  <stuff category=\"112\">\n    <name>Target</name>\n    <version>1.0</version>\n    <installedfile>/home/u/target/Target.colors</installedfile>\n    <id>222</id>\n  </stuff>\n</hotnewstuffregistry>\n"
        );

        let out = update_entry(
            &xml,
            &update_fields("Target.colors", Path::new("/home/u/target/Target.colors")),
        )
        .unwrap()
        .unwrap();

        assert!(
            out.contains(foreign),
            "untouched entry not preserved:\n{out}"
        );
    }

    #[test]
    fn add_entry_treats_an_empty_or_whitespace_registry_as_fresh() {
        for blank in ["", "   ", "\n\t \n"] {
            let out = add_entry(blank, &new_entry(Path::new("/tmp/a"))).unwrap();

            assert!(
                out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
                "{blank:?} -> {out:?}"
            );
            let parsed = parse_raw_entries(&out).unwrap();
            assert_eq!(parsed.len(), 1, "{blank:?} -> {out}");
            assert_eq!(parsed[0].name, "Fake & Caf<é>");
        }
    }

    #[test]
    fn cdata_content_is_read_as_text() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name><![CDATA[Fake & Caf<é>]]></name>
    <installedfile><![CDATA[/home/u/my&dir/f/metadata.json]]></installedfile>
    <id>7</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let entries = parse_raw_entries(xml).unwrap();

        assert_eq!(entries[0].name, "Fake & Caf<é>");
        assert_eq!(
            entries[0].installed_files,
            vec!["/home/u/my&dir/f/metadata.json"]
        );
    }

    #[test]
    fn update_rewrites_a_self_closing_field() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Target</name>
    <version/>
    <installedfile>/home/u/target/Target.colors</installedfile>
    <id>222</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let out = update_entry(
            xml,
            &update_fields("Target.colors", Path::new("/home/u/target/replacement")),
        )
        .unwrap()
        .unwrap();

        assert!(out.contains("<version>2.0</version>"), "{out}");
        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed[0].version, "2.0");
    }

    #[test]
    fn update_rewrites_a_field_holding_nested_markup() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Target</name>
    <version><b>1.0</b></version>
    <installedfile>/home/u/target/Target.colors</installedfile>
    <id>222</id>
  </stuff>
</hotnewstuffregistry>
"#;

        let out = update_entry(
            xml,
            &update_fields("Target.colors", Path::new("/home/u/target/replacement")),
        )
        .unwrap()
        .unwrap();

        assert!(out.contains("<version>2.0</version>"), "{out}");
        assert!(!out.contains("<b>"), "{out}");
        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed[0].version, "2.0");
    }

    #[test]
    fn add_entry_inserts_before_the_root_element_found_by_the_parser() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<hotnewstuffregistry>\n  <stuff category=\"112\">\n    <name>Foreign</name>\n    <installedfile>/home/u/foreign/metadata.json</installedfile>\n    <id>1</id>\n  </stuff>\n</hotnewstuffregistry>\n<!-- a note mentioning </hotnewstuffregistry> -->\n";

        let out = add_entry(xml, &new_entry(Path::new("/tmp/a"))).unwrap();

        assert!(
            out.contains("<!-- a note mentioning </hotnewstuffregistry> -->"),
            "{out}"
        );
        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed.len(), 2, "{out}");
        assert_eq!(parsed[1].name, "Fake & Caf<é>");
    }

    #[test]
    fn add_entry_writes_a_full_document_for_a_self_closing_root() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<hotnewstuffregistry/>\n";

        let out = add_entry(xml, &new_entry(Path::new("/tmp/a"))).unwrap();

        assert!(
            out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
            "{out}"
        );
        assert!(out.contains("<hotnewstuffregistry>"), "{out}");
        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed.len(), 1, "{out}");
        assert_eq!(parsed[0].name, "Fake & Caf<é>");
    }

    #[test]
    fn a_registry_holding_only_a_byte_order_mark_is_fresh() {
        for blank in ["\u{feff}", "\u{feff}\n", "\u{feff} \n\t"] {
            assert!(
                parse_raw_entries(blank).unwrap().is_empty(),
                "{blank:?} parsed as a registry with entries"
            );
            let out = add_entry(blank, &new_entry(Path::new("/tmp/a"))).unwrap();
            assert!(
                out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
                "{blank:?} -> {out:?}"
            );
            assert_eq!(
                parse_raw_entries(&out).unwrap().len(),
                1,
                "{blank:?} -> {out}"
            );
        }
    }

    #[test]
    fn update_leaves_a_root_level_element_outside_the_entry_alone() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<hotnewstuffregistry>
  <stuff category="112">
    <name>Target</name>
    <version>1.0</version>
    <installedfile>/home/u/target/Target.colors</installedfile>
    <id>222</id>
  </stuff>
  <version>9.9</version>
  <status>foreign</status>
</hotnewstuffregistry>
"#;

        let out = update_entry(
            xml,
            &update_fields("Target.colors", Path::new("/home/u/target/replacement")),
        )
        .unwrap()
        .unwrap();

        let parsed = parse_raw_entries(&out).unwrap();
        assert_eq!(parsed[0].version, "2.0", "{out}");
        assert!(
            out.contains("<version>9.9</version>"),
            "the root-level element was rewritten:\n{out}"
        );
        assert!(
            out.contains("<status>foreign</status>"),
            "the root-level element was rewritten:\n{out}"
        );
    }

    #[test]
    fn text_without_the_root_element_is_a_parse_error() {
        let err = parse_registry_entries("not a registry\n").unwrap_err();
        assert!(matches!(err, Error::Parse { .. }), "{err}");

        let err = parse_registry_entries("\0\0\0").unwrap_err();
        assert!(matches!(err, Error::Parse { .. }), "{err}");

        assert!(parse_raw_entries("").unwrap().is_empty());
        assert!(parse_raw_entries("  \n").unwrap().is_empty());
    }
}
