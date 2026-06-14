//! Plain-text parser: catch-all wrapper around `crate::plaintext::extract_plaintext`.
//!
//! Accepts any input that is valid UTF-8. Place this **last** in the chain so
//! that more specific parsers (Office, HTML, Markdown, PDF) get first pick.

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;

/// Catch-all parser: accepts any valid UTF-8 byte sequence.
///
/// Returns `Ok(None)` for non-UTF-8 bytes (binary content).
pub struct PlaintextParser;

impl Parser for PlaintextParser {
    fn id(&self) -> &'static str {
        "plaintext"
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        let text = match std::str::from_utf8(probe.bytes()) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };

        let (markdown, title) = crate::plaintext::extract_plaintext(text)?;

        let mut dc = DocumentMetadata::default();
        if let Some(mime) = probe.sniffed_mime {
            dc.format = Some(mime.to_string());
        }

        Ok(Some(ParsedDocument {
            markdown,
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
    fn accepts_utf8_text() {
        let probe = Probe::new(b"Hello, world!", None, None);
        let doc = PlaintextParser.parse(&probe).unwrap().unwrap();
        assert!(doc.markdown.contains("Hello"));
    }

    #[test]
    fn accepts_markdown_as_text_when_placed_first() {
        let probe = Probe::new(b"# Heading\n\nParagraph.", None, None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn declines_binary_non_utf8() {
        let binary = b"\xFF\xFE\x00\x01binary content";
        let probe = Probe::new(binary, None, None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn accepts_any_extension() {
        for ext in &["txt", "csv", "log", "rs", "json"] {
            let filename = format!("file.{ext}");
            let probe = Probe::new(b"Some content", Some(&filename), None);
            assert!(
                PlaintextParser.parse(&probe).unwrap().is_some(),
                "PlaintextParser should accept .{ext}"
            );
        }
    }

    #[test]
    fn accepts_no_extension() {
        let probe = Probe::new(b"Plain text content", None, None);
        assert!(PlaintextParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn sniffed_mime_populates_dc_format() {
        let probe = Probe::new(b"plain text", None, Some("text/plain"));
        let doc = PlaintextParser.parse(&probe).unwrap().unwrap();
        assert_eq!(doc.metadata.format, Some("text/plain".to_string()));
    }
}
