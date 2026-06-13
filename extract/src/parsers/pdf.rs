//! PDF parser: chain-of-responsibility wrapper around `crate::pdf::extract_pdf`.

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;

/// Handles PDFs identified by magic bytes (`%PDF-`) or the `.pdf` extension.
///
/// Declines all other inputs. Scanned PDFs (no text layer) return `Err`,
/// short-circuiting the chain so plaintext does not silently grab them.
pub struct PdfParser;

impl Parser for PdfParser {
    fn id(&self) -> &'static str {
        "pdf"
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        let by_magic = probe.header().starts_with(b"%PDF-");
        let by_ext = probe
            .extension()
            .map(|e| e.eq_ignore_ascii_case("pdf"))
            .unwrap_or(false);

        if !by_magic && !by_ext {
            return Ok(None);
        }

        let out = crate::pdf::extract_pdf(probe.bytes())?;

        let mut dc = DocumentMetadata::default();
        if let Some(mime) = probe.sniffed_mime {
            dc.format = Some(mime.to_string());
        }

        Ok(Some(ParsedDocument {
            text: out.text,
            blocks: out.blocks,
            title: out.title,
            metadata: dc,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::parser::Probe;

    #[test]
    fn declines_plaintext() {
        let probe = Probe::new(b"Hello, world!", Some("notes.txt"), None);
        assert!(PdfParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_no_hint() {
        let probe = Probe::new(b"Hello, world!", None, None);
        assert!(PdfParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn accepts_pdf_magic() {
        // A minimal valid-looking PDF header triggers acceptance (actual parse
        // may fail or succeed depending on content).
        let bytes = b"%PDF-1.4\n%%EOF\n";
        let probe = Probe::new(bytes, Some("doc.txt"), None); // wrong ext, magic wins
        let result = PdfParser.parse(&probe);
        // Either Ok(Some) or Err (scanned), but NOT Ok(None)
        assert!(result.is_ok() || result.is_err());
        if let Ok(v) = result {
            assert!(v.is_some(), "magic-matched PDF should not return Ok(None)");
        }
    }

    #[test]
    fn accepts_pdf_extension() {
        let bytes = b"%PDF-1.4\nsome content here with enough printable characters to pass threshold\n%%EOF\n";
        let probe = Probe::new(bytes, Some("report.pdf"), None);
        let result = PdfParser.parse(&probe);
        assert!(result.is_ok() || result.is_err());
        if let Ok(v) = result {
            assert!(v.is_some());
        }
    }

    #[test]
    fn declines_html_extension() {
        let probe = Probe::new(b"<html><body>text</body></html>", Some("page.html"), None);
        assert!(PdfParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_md_extension() {
        let probe = Probe::new(b"# Heading\n\nParagraph.", Some("README.md"), None);
        assert!(PdfParser.parse(&probe).unwrap().is_none());
    }
}
