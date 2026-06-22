//! Office document parser: DOCX, PPTX, CSV → Markdown via `anytomd`.
//!
//! XLSX and XLS are intentionally excluded: anytomd's spreadsheet-to-Markdown
//! conversion is extremely slow on files with thousands of rows (measured at
//! over 16 minutes in production for an 87K-row file; use CSV export instead).
//! Tracking issue: <https://github.com/developer0hye/anytomd-rs/issues/94>

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;

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

        let title = result.title.clone();
        let dc = DocumentMetadata {
            title: title.clone(),
            format: probe.sniffed_mime.map(|s| s.to_string()),
            ..DocumentMetadata::default()
        };

        Ok(Some(ParsedDocument {
            markdown: result.markdown,
            title,
            metadata: dc,
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
