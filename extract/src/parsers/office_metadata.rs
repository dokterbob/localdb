//! `docProps/core.xml` extraction for OOXML (docx/pptx) packages (issue #251).
//!
//! `.csv` has no `docProps` part (it isn't a zip container at all) and `.odt`
//! has no parser yet (#254), so this module only ever runs for docx/pptx.

use std::io::{Cursor, Read};

use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// Explicit metadata read from an OOXML package's `docProps/core.xml`.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct OfficeCoreProperties {
    /// `dc:title`, trimmed. `None` when the element is absent OR
    /// present-but-empty (`<dc:title/>` or `<dc:title></dc:title>`) —
    /// callers treat empty the same as absent (empty is not an authoritative
    /// title claim).
    pub title: Option<String>,
    /// `dcterms:created`, trimmed, stored raw (not date-parsed here) — per
    /// the `date_original` convention, the raw claim is kept and downstream
    /// `date_parsed` derivation (`core::dates::parse_partial_iso8601`)
    /// normalizes it later.
    ///
    /// ECMA-376 Part 1 §15.2.12.1 defines `dcterms:created` as the
    /// resource's creation date/time; a bare `dc:date` is a distinct,
    /// generic Dublin Core element that Word/PowerPoint do not populate in
    /// `core.xml` in practice, so only `dcterms:created` is read.
    pub created: Option<String>,
}

/// Read `docProps/core.xml` from raw OOXML (docx/pptx) bytes.
///
/// Returns `None` for anything that isn't a cleanly readable, well-formed
/// `docProps/core.xml`: a malformed/non-zip archive, a missing part, or
/// malformed XML. Never returns `Err` — this reads untrusted document
/// content, and a metadata-extraction failure must never block extracting
/// the document body (the `anytomd` conversion the caller also runs).
pub(crate) fn read_core_properties(bytes: &[u8]) -> Option<OfficeCoreProperties> {
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).ok()?;
    let mut raw = Vec::new();
    // Cap the decompressed read: a legitimate core.xml is a few KB, and this
    // reads untrusted content — an over-cap part is truncated and then fails
    // XML parsing below, landing in the same fail-closed `None` path.
    archive
        .by_name("docProps/core.xml")
        .ok()?
        .take(1 << 20)
        .read_to_end(&mut raw)
        .ok()?;
    // Some OOXML producers write core.xml as UTF-16. `UTF_8.decode` BOM-sniffs
    // UTF-8/UTF-16LE/UTF-16BE per WHATWG, strips the BOM, and never errors —
    // a part with no BOM is decoded as UTF-8, matching the previous behavior.
    let (xml, _, _) = encoding_rs::UTF_8.decode(&raw);
    parse_core_properties(&xml)
}

/// Field currently being accumulated (`None` outside `title`/`created`, or
/// inside any other element — nested/unknown elements are ignored).
enum Field {
    Title,
    Created,
}

/// Parse a `docProps/core.xml` document body.
///
/// Matches purely on each element's local name (namespace-prefix-agnostic —
/// `dc:title` and `cp:title` are both recognized as `title`), since OOXML
/// producers are consistent about which prefix maps to which namespace but
/// this extractor has no reason to police that. Malformed XML → `None`, not
/// an error.
fn parse_core_properties(xml: &str) -> Option<OfficeCoreProperties> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut props = OfficeCoreProperties::default();
    let mut current: Option<Field> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                current = match e.name().local_name().as_ref() {
                    b"title" => Some(Field::Title),
                    b"created" => Some(Field::Created),
                    _ => None,
                };
            }
            // `<dc:title/>` — empty element: no Text event follows, so the
            // corresponding field is simply never assigned, staying `None`.
            Ok(Event::Empty(_)) => {}
            Ok(Event::Text(t)) => {
                if current.is_some() {
                    let decoded = t.decode().ok()?;
                    let text = quick_xml::escape::unescape(&decoded).ok()?.into_owned();
                    assign_field(&current, &text, &mut props);
                }
            }
            // CDATA content is already literal — unlike `Event::Text`, it must
            // NOT be run through entity-unescape, or a literal '&' would be
            // corrupted (or error out as a malformed entity reference).
            Ok(Event::CData(t)) => {
                if current.is_some() {
                    let text = std::str::from_utf8(t.as_ref()).ok()?;
                    assign_field(&current, text, &mut props);
                }
            }
            Ok(Event::End(_)) => current = None,
            Err(_) => return None,
            _ => {}
        }
    }

    Some(props)
}

/// Assign already-decoded `text` to whichever field `current` names, if any,
/// once trimmed non-empty. Shared tail for the `Text` and `CData` arms, which
/// differ only in how they turn their raw event bytes into `text`.
fn assign_field(current: &Option<Field>, text: &str, props: &mut OfficeCoreProperties) {
    if let Some(field) = current {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            match field {
                Field::Title => props.title = Some(trimmed.to_string()),
                Field::Created => props.created = Some(trimmed.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build a minimal in-memory OOXML zip containing (or omitting) a
    /// `docProps/core.xml` part with the given body.
    fn make_ooxml_zip(core_xml: Option<&str>) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let buf = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buf);
        let opts = SimpleFileOptions::default();

        // A minimal but plausible-looking package around the part under test.
        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types/>"#).unwrap();

        if let Some(core_xml) = core_xml {
            zip.start_file("docProps/core.xml", opts).unwrap();
            zip.write_all(core_xml.as_bytes()).unwrap();
        }

        zip.finish().unwrap().into_inner()
    }

    const CORE_XML_FULL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:dcterms="http://purl.org/dc/terms/"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <dc:title>Q3 Board Report</dc:title>
  <dcterms:created xsi:type="dcterms:W3CDTF">2019-03-04T00:00:00Z</dcterms:created>
</cp:coreProperties>"#;

    #[test]
    fn extracts_dcterms_created() {
        let props = parse_core_properties(CORE_XML_FULL).unwrap();
        assert_eq!(props.created.as_deref(), Some("2019-03-04T00:00:00Z"));
    }

    #[test]
    fn title_fills_from_dc_title() {
        let props = parse_core_properties(CORE_XML_FULL).unwrap();
        assert_eq!(props.title.as_deref(), Some("Q3 Board Report"));
    }

    #[test]
    fn empty_title_element_treated_as_absent() {
        let xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
    xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/">
  <dc:title/>
  <dcterms:created>2020-06-01T00:00:00Z</dcterms:created>
</cp:coreProperties>"#;
        let props = parse_core_properties(xml).unwrap();
        assert_eq!(
            props.title, None,
            "empty <dc:title/> must be treated as absent"
        );
        assert_eq!(props.created.as_deref(), Some("2020-06-01T00:00:00Z"));
    }

    #[test]
    fn missing_core_xml_does_not_error() {
        let bytes = make_ooxml_zip(None);
        assert_eq!(read_core_properties(&bytes), None);
    }

    #[test]
    fn missing_docprops_part_in_valid_zip_returns_none() {
        // Sanity: a well-formed zip without the part at all, exercised
        // through the full `read_core_properties` (zip-open) path rather
        // than the XML-string path the other tests use.
        let bytes = make_ooxml_zip(None);
        assert!(read_core_properties(&bytes).is_none());
    }

    #[test]
    fn malformed_xml_does_not_error() {
        // Mismatched end-tag name — `check_end_names` (on by default) makes
        // quick-xml surface this as an `Err`, which the parser must turn
        // into `None`, not propagate.
        let bytes = make_ooxml_zip(Some(
            "<cp:coreProperties><dc:title>Test</dc:oops></cp:coreProperties>",
        ));
        assert_eq!(read_core_properties(&bytes), None);
    }

    #[test]
    fn malformed_zip_does_not_error() {
        assert_eq!(
            read_core_properties(b"this is not a zip file at all!"),
            None
        );
    }

    #[test]
    fn round_trip_via_zip_extracts_both_fields() {
        let bytes = make_ooxml_zip(Some(CORE_XML_FULL));
        let props = read_core_properties(&bytes).unwrap();
        assert_eq!(props.title.as_deref(), Some("Q3 Board Report"));
        assert_eq!(props.created.as_deref(), Some("2019-03-04T00:00:00Z"));
    }

    /// Byte-taking variant of `make_ooxml_zip`, for parts whose bytes are not
    /// valid UTF-8 (e.g. UTF-16 `core.xml`).
    fn make_ooxml_zip_bytes(core_xml: Option<&[u8]>) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;

        let buf = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buf);
        let opts = SimpleFileOptions::default();

        zip.start_file("[Content_Types].xml", opts).unwrap();
        zip.write_all(br#"<?xml version="1.0"?><Types/>"#).unwrap();

        if let Some(core_xml) = core_xml {
            zip.start_file("docProps/core.xml", opts).unwrap();
            zip.write_all(core_xml).unwrap();
        }

        zip.finish().unwrap().into_inner()
    }

    fn utf16le_bytes_with_bom(s: &str) -> Vec<u8> {
        let mut out = vec![0xFFu8, 0xFEu8];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    fn utf16be_bytes_with_bom(s: &str) -> Vec<u8> {
        let mut out = vec![0xFEu8, 0xFFu8];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        out
    }

    #[test]
    fn core_xml_utf16le_bom_is_decoded() {
        let bytes = make_ooxml_zip_bytes(Some(&utf16le_bytes_with_bom(CORE_XML_FULL)));
        let props = read_core_properties(&bytes).unwrap();
        assert_eq!(props.title.as_deref(), Some("Q3 Board Report"));
        assert_eq!(props.created.as_deref(), Some("2019-03-04T00:00:00Z"));
    }

    #[test]
    fn core_xml_utf16be_bom_is_decoded() {
        let bytes = make_ooxml_zip_bytes(Some(&utf16be_bytes_with_bom(CORE_XML_FULL)));
        let props = read_core_properties(&bytes).unwrap();
        assert_eq!(props.title.as_deref(), Some("Q3 Board Report"));
        assert_eq!(props.created.as_deref(), Some("2019-03-04T00:00:00Z"));
    }

    #[test]
    fn cdata_title_extracted() {
        let xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
    xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/">
  <dc:title><![CDATA[Q3 Board Report]]></dc:title>
</cp:coreProperties>"#;
        let props = parse_core_properties(xml).unwrap();
        assert_eq!(props.title.as_deref(), Some("Q3 Board Report"));
    }

    #[test]
    fn cdata_created_extracted() {
        let xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
    xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/">
  <dcterms:created><![CDATA[2019-03-04T00:00:00Z]]></dcterms:created>
</cp:coreProperties>"#;
        let props = parse_core_properties(xml).unwrap();
        assert_eq!(props.created.as_deref(), Some("2019-03-04T00:00:00Z"));
    }

    #[test]
    fn cdata_ampersand_not_double_unescaped() {
        // A literal '&' inside CDATA must survive as-is: CDATA is not an
        // escaped context, so re-running entity-unescape on it would either
        // corrupt the literal '&' or error out on "& Q2" as a malformed
        // entity reference.
        let xml = r#"<?xml version="1.0"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
    xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:dcterms="http://purl.org/dc/terms/">
  <dc:title><![CDATA[Q1 & Q2 Report]]></dc:title>
</cp:coreProperties>"#;
        let props = parse_core_properties(xml).unwrap();
        assert_eq!(props.title.as_deref(), Some("Q1 & Q2 Report"));
    }
}
