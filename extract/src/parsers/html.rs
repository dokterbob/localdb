//! HTML parser: chain-of-responsibility wrapper around `crate::html::extract_html`.

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;

/// Handles HTML identified by extension (`.html`, `.htm`, `.xhtml`) or a
/// leading `<` heuristic.
///
/// Declines inputs that show no HTML signal. Does NOT short-circuit on error —
/// HTML parsing is lenient (scraper never fails on malformed input).
pub struct HtmlParser;

impl Parser for HtmlParser {
    fn id(&self) -> &'static str {
        "html"
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        let by_ext = probe
            .extension()
            .map(|e| matches!(e.to_lowercase().as_str(), "html" | "htm" | "xhtml"))
            .unwrap_or(false);

        let by_heuristic = {
            let header = probe.header();
            let first_nonspace = header.iter().position(|&b| !b.is_ascii_whitespace());
            first_nonspace
                .map(|pos| header[pos..].starts_with(b"<"))
                .unwrap_or(false)
        };

        if !by_ext && !by_heuristic {
            return Ok(None);
        }

        let text = std::str::from_utf8(probe.bytes()).map_err(|e| Error::InvalidRequest {
            message: format!("HTML is not valid UTF-8: {e}"),
        })?;

        let out = crate::html::extract_html(text)?;

        let dc = DocumentMetadata {
            title: out.title.clone(),
            format: probe.sniffed_mime.map(|s| s.to_string()),
            ..DocumentMetadata::default()
        };

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
    fn declines_plaintext_no_angle() {
        let probe = Probe::new(b"Hello, world!", Some("notes.txt"), None);
        assert!(HtmlParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_md_extension() {
        let probe = Probe::new(b"# Heading\n\nParagraph.", Some("README.md"), None);
        assert!(HtmlParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn accepts_html_extension() {
        let probe = Probe::new(
            b"<html><body><p>Hello</p></body></html>",
            Some("page.html"),
            None,
        );
        let doc = HtmlParser.parse(&probe).unwrap().unwrap();
        assert!(doc.text.contains("Hello"));
    }

    #[test]
    fn accepts_htm_extension() {
        let probe = Probe::new(
            b"<html><body><p>Hi</p></body></html>",
            Some("page.htm"),
            None,
        );
        assert!(HtmlParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_xhtml_extension() {
        let probe = Probe::new(
            b"<html><body><p>Hi</p></body></html>",
            Some("page.xhtml"),
            None,
        );
        assert!(HtmlParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_html_heuristic_no_extension() {
        let probe = Probe::new(b"<html><body><p>Content</p></body></html>", None, None);
        assert!(HtmlParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_doctype_heuristic() {
        let probe = Probe::new(
            b"<!DOCTYPE html><html><body><p>Doc</p></body></html>",
            None,
            None,
        );
        assert!(HtmlParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn html_title_populates_dc_title() {
        let html = b"<html><head><title>My Page</title></head><body><p>Content</p></body></html>";
        let probe = Probe::new(html, Some("page.html"), None);
        let doc = HtmlParser.parse(&probe).unwrap().unwrap();
        assert_eq!(doc.metadata.title, Some("My Page".to_string()));
        assert_eq!(doc.title, doc.metadata.title);
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        let probe = Probe::new(
            b"<html><body><p>Hi</p></body></html>",
            Some("page.HTML"),
            None,
        );
        assert!(HtmlParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn declines_whitespace_only_input_no_hint() {
        let probe = Probe::new(b"   \n   ", None, None);
        assert!(HtmlParser.parse(&probe).unwrap().is_none());
    }
}
