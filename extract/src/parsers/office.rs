//! Office document parser: DOCX, PPTX, CSV → Markdown via `anytomd`.
//!
//! XLSX and XLS are intentionally excluded: anytomd's spreadsheet-to-Markdown
//! conversion is extremely slow on files with thousands of rows (measured at
//! over 16 minutes in production for an 87K-row file; use CSV export instead).
//! Tracking issue: <https://github.com/developer0hye/anytomd-rs/issues/94>

use localdb_core::metadata::DublinCoreMetadata;
use localdb_core::parser::{ParsedDocument, Parser, Probe};
use localdb_core::Error;

use super::office_metadata::read_core_properties;

/// Handles office document formats via `anytomd`.
///
/// Supported extensions: `.docx`, `.pptx`, `.csv`.
/// Declines all other inputs, including `.xlsx` and `.xls` (disabled — see module docs).
pub struct OfficeParser;

/// Office file extensions handled by this parser.
///
/// `.xlsx` and `.xls` are intentionally absent — see module-level doc comment.
const OFFICE_EXTS: &[&str] = &["docx", "pptx", "csv"];

impl Parser for OfficeParser {
    fn id(&self) -> &'static str {
        "office"
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        let ext = match probe.extension().map(|e| e.to_lowercase()) {
            Some(e) if OFFICE_EXTS.contains(&e.as_str()) => e,
            _ => return Ok(None),
        };

        let opts = anytomd::ConversionOptions::default();
        let result = anytomd::convert_bytes(probe.bytes(), &ext, &opts).map_err(|e| {
            Error::ExtractionFailed {
                format: format!("office/{ext}"),
                reason: e.to_string(),
            }
        })?;

        // docProps/core.xml only exists for docx/pptx (both OOXML zip
        // packages); csv is plain text and has no such part — skip the
        // zip-open attempt for it entirely.
        let core_props = if ext == "csv" {
            None
        } else {
            read_core_properties(probe.bytes())
        };

        // Title precedence is explicit-over-heuristic: core.xml's dc:title,
        // trimmed and non-empty, wins over anytomd's derived title outright
        // — even for a stale placeholder like Word's default "Document1"
        // (a *quality-aware* precedence would prefer a good heading instead,
        // but that tradeoff is deliberately not made here; see
        // extract/tests/metadata_extraction.rs's docx-junk-title case).
        // Empty/whitespace dc:title (`<dc:title/>`) is already normalized to
        // `None` by `office_metadata::parse_core_properties`, so it falls
        // through to anytomd's title with no special-casing needed here.
        let core_title = core_props.as_ref().and_then(|p| p.title.clone());
        let title = core_title.or_else(|| result.title.clone());

        let (date, date_source) = match core_props.and_then(|p| p.created) {
            Some(created) => (Some(created), Some("office-core-properties".to_string())),
            None => (None, None),
        };

        let dc = DublinCoreMetadata {
            title: title.clone(),
            date,
            date_source,
            format: probe.sniffed_mime.map(|s| s.to_string()),
            ..DublinCoreMetadata::default()
        };

        Ok(Some(ParsedDocument {
            markdown: result.markdown,
            title,
            metadata: dc,
            page_starts: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::parser::Probe;

    #[test]
    fn declines_pdf_extension() {
        let probe = Probe::new(b"%PDF-1.4\n", Some("doc.pdf"), None);
        assert!(OfficeParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_md_extension() {
        let probe = Probe::new(b"# Hello", Some("README.md"), None);
        assert!(OfficeParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_no_extension() {
        let probe = Probe::new(b"some content", None, None);
        assert!(OfficeParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn csv_is_converted_to_markdown() {
        let csv = b"Name,Age\nAlice,30\nBob,25\n";
        let probe = Probe::new(csv, Some("data.csv"), None);
        let doc = OfficeParser.parse(&probe).unwrap().unwrap();
        assert!(
            !doc.markdown.is_empty(),
            "CSV should produce non-empty markdown"
        );
        // anytomd converts CSV to a markdown table
        assert!(
            doc.markdown.contains("Alice") || doc.markdown.contains("Name"),
            "CSV content should appear in markdown: {}",
            &doc.markdown[..doc.markdown.len().min(200)]
        );
    }

    #[test]
    fn declines_html_extension() {
        let probe = Probe::new(b"<html>...</html>", Some("page.html"), None);
        assert!(OfficeParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn garbage_docx_returns_extraction_failed() {
        let probe = Probe::new(b"this is not a zip file at all!", Some("doc.docx"), None);
        match OfficeParser.parse(&probe) {
            Err(Error::ExtractionFailed { format, .. }) => {
                assert!(
                    format.starts_with("office/docx"),
                    "unexpected format: {format}"
                );
            }
            other => panic!("expected ExtractionFailed, got: {other:?}"),
        }
    }

    #[test]
    fn xlsx_returns_none_disabled() {
        // XLSX is intentionally disabled (anytomd performance bug #94).
        // The parser must return Ok(None) so the file is counted as
        // unsupported_format, not as an extraction error.
        let probe = Probe::new(b"\x00\x01\x02\x03garbage", Some("sheet.xlsx"), None);
        assert!(
            OfficeParser.parse(&probe).unwrap().is_none(),
            "xlsx is disabled and should return Ok(None)"
        );
    }

    #[test]
    fn xls_returns_none_disabled() {
        // XLS is intentionally disabled for the same reason as XLSX.
        let probe = Probe::new(b"\xd0\xcf\x11\xe0garbage", Some("sheet.xls"), None);
        assert!(
            OfficeParser.parse(&probe).unwrap().is_none(),
            "xls is disabled and should return Ok(None)"
        );
    }
}
