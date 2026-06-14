//! Office document parser: DOCX, PPTX, XLSX, XLS, CSV → Markdown via `anytomd`.
//!
//! anytomd emits GFM tables for spreadsheets; `MarkdownSplitter` handles them.

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;

/// Handles office document formats via `anytomd`.
///
/// Supported extensions: `.docx`, `.pptx`, `.xlsx`, `.xls`, `.csv`.
/// Declines all other inputs.
pub struct OfficeParser;

/// Office file extensions handled by this parser.
const OFFICE_EXTS: &[&str] = &["docx", "pptx", "xlsx", "xls", "csv"];

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
            Error::UnsupportedFormat {
                format: format!("office/{ext} (anytomd: {e})"),
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
}
