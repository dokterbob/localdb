//! Markdown parser: chain-of-responsibility wrapper around
//! `crate::markdown::extract_markdown`.

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;

/// Handles Markdown identified by a known extension.
///
/// Recognized extensions: `.md`, `.markdown`, `.mdown`, `.mkd`, `.mkdn`.
/// Declines all other inputs (no content heuristic — Markdown has no magic).
/// Also declines non-UTF-8 bytes (binary / mis-encoded files) with `Ok(None)`
/// so they fall through to `UnsupportedFormat`, not an error.
pub struct MarkdownParser;

impl Parser for MarkdownParser {
    fn id(&self) -> &'static str {
        "markdown"
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        let accepted = probe
            .extension()
            .map(|e| {
                matches!(
                    e.to_lowercase().as_str(),
                    "md" | "markdown" | "mdown" | "mkd" | "mkdn"
                )
            })
            .unwrap_or(false);

        if !accepted {
            return Ok(None);
        }

        let text = match std::str::from_utf8(probe.bytes()) {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };

        let out = crate::markdown::extract_markdown(text)?;

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
    fn declines_no_extension() {
        let probe = Probe::new(b"# Hello\n\nParagraph.", None, None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_txt_extension() {
        let probe = Probe::new(b"# Hello\n\nParagraph.", Some("notes.txt"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_html_extension() {
        let probe = Probe::new(b"<html><body>hi</body></html>", Some("page.html"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn accepts_md_extension() {
        let probe = Probe::new(b"# Hello\n\nParagraph.", Some("README.md"), None);
        let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
        assert!(doc.text.contains("Hello"));
    }

    #[test]
    fn accepts_markdown_extension() {
        let probe = Probe::new(b"# Hello", Some("notes.markdown"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_mdown_extension() {
        let probe = Probe::new(b"# Hello", Some("notes.mdown"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_mkd_extension() {
        let probe = Probe::new(b"# Hello", Some("notes.mkd"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn accepts_mkdn_extension() {
        let probe = Probe::new(b"# Hello", Some("notes.mkdn"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        let probe = Probe::new(b"# Hello", Some("README.MD"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn declines_binary_non_utf8() {
        let binary = b"\xFF\xFE\x00\x01binary content";
        let probe = Probe::new(binary, Some("file.md"), None);
        assert!(MarkdownParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn h1_heading_populates_dc_title() {
        let probe = Probe::new(b"# My Document\n\nSome content.", Some("doc.md"), None);
        let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
        assert_eq!(doc.metadata.title, Some("My Document".to_string()));
    }
}
