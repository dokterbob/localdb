//! Extensible document parser trait and composite dispatcher.
//!
//! The `Parser` trait is the core abstraction for format-specific extraction:
//! - `Ok(None)` — decline; pass control to the next parser in the chain.
//! - `Ok(Some(doc))` — handled successfully.
//! - `Err(e)` — format was recognized but parsing failed; short-circuits the chain.
//!
//! `ChainParser` implements the chain-of-responsibility pattern and is itself
//! a `Parser`, enabling nesting and swappable selection strategies.

use crate::{Block, Error};

/// Leading bytes exposed for cheap format sniffing without seeking.
pub const PROBE_HEADER_LEN: usize = 8192;

/// Fully-buffered input presented to each parser.
///
/// The streaming or HTTPS read happens once at the ingestion boundary.
/// Parsers call `header()` for cheap magic-byte checks and `bytes()` for
/// the full content. No seeking is required.
pub struct Probe<'a> {
    bytes: &'a [u8],
    /// Original filename or URL path, used for extension hints.
    pub path_hint: Option<&'a str>,
    /// Advisory MIME type inferred before parsing (may be `None` or wrong).
    pub sniffed_mime: Option<&'a str>,
}

impl<'a> Probe<'a> {
    pub fn new(bytes: &'a [u8], path_hint: Option<&'a str>, sniffed_mime: Option<&'a str>) -> Self {
        Self {
            bytes,
            path_hint,
            sniffed_mime,
        }
    }

    /// Up to `PROBE_HEADER_LEN` leading bytes for cheap sniffing.
    pub fn header(&self) -> &[u8] {
        &self.bytes[..self.bytes.len().min(PROBE_HEADER_LEN)]
    }

    /// Full document bytes.
    pub fn bytes(&self) -> &[u8] {
        self.bytes
    }

    /// File extension from `path_hint`, lowercase, without the leading dot.
    /// Returns `None` if there is no path hint or no extension separator.
    pub fn extension(&self) -> Option<&str> {
        self.path_hint.and_then(|p| {
            // Take the last path component, then split on '.'.
            let basename = p.rsplit('/').next().unwrap_or(p);
            // Only return the part after the last '.' if there IS a '.' in basename
            // and it is not the first character (hidden files like ".gitignore").
            if let Some(dot) = basename.rfind('.') {
                if dot > 0 {
                    return Some(&basename[dot + 1..]);
                }
            }
            None
        })
    }
}

// ---------------------------------------------------------------------------
// ParsedDocument + DocumentMetadata
// ---------------------------------------------------------------------------

/// A parsed document: normalized text, structural blocks, and metadata.
#[derive(Debug, Clone, Default)]
pub struct ParsedDocument {
    /// Normalized document text. All block spans index into this.
    pub text: String,
    /// Structural blocks in document order.
    pub blocks: Vec<Block>,
    /// Title from extraction (kept as a typed fast-path).
    pub title: Option<String>,
    /// Document metadata extracted from the document.
    pub metadata: DocumentMetadata,
}

/// Dublin Core Metadata Element Set 1.1 (DCMES), all 15 elements.
///
/// Repeatable elements use `Vec`; singleton elements use `Option<String>`.
/// Persisted as a single JSON-encoded UTF-8 column in LanceDB.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub creator: Vec<String>,
    pub subject: Vec<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub contributor: Vec<String>,
    pub date: Option<String>,
    pub r#type: Option<String>,
    pub format: Option<String>,
    pub identifier: Option<String>,
    pub source: Option<String>,
    pub language: Option<String>,
    pub relation: Vec<String>,
    pub coverage: Option<String>,
    pub rights: Option<String>,
}

// ---------------------------------------------------------------------------
// Parser trait
// ---------------------------------------------------------------------------

/// A document parser in the chain of responsibility.
///
/// Implementors inspect the `Probe` and either decline (`Ok(None)`), handle
/// (`Ok(Some(doc))`), or fail a recognized format (`Err(e)`). The trait is
/// **sync** (CPU-bound); callers run it under `spawn_blocking`.
pub trait Parser: Send + Sync {
    /// Stable id used in the config `parsers:` list and in diagnostics.
    fn id(&self) -> &'static str;

    /// Try to parse the input document.
    ///
    /// - `Ok(None)` — decline; pass control to the next parser in the chain.
    /// - `Ok(Some(doc))` — handled successfully.
    /// - `Err(e)` — format was recognized but parsing failed; short-circuits.
    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error>;
}

// ---------------------------------------------------------------------------
// ChainParser — strict first-match composite
// ---------------------------------------------------------------------------

/// Strict first-match composite parser.
///
/// Tries parsers in declaration order, returning the first `Ok(Some)` result.
/// An `Err` short-circuits — remaining parsers are NOT tried, because the
/// format was recognized (and specifically failed), not "unknown".
///
/// `ChainParser` is itself a `Parser`, so it can be nested or swapped.
pub struct ChainParser {
    id: &'static str,
    parsers: Vec<Box<dyn Parser>>,
}

impl std::fmt::Debug for ChainParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainParser")
            .field("id", &self.id)
            .field(
                "parsers",
                &self.parsers.iter().map(|p| p.id()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ChainParser {
    pub fn new(id: &'static str, parsers: Vec<Box<dyn Parser>>) -> Self {
        Self { id, parsers }
    }

    pub fn parsers(&self) -> &[Box<dyn Parser>] {
        &self.parsers
    }
}

impl Parser for ChainParser {
    fn id(&self) -> &'static str {
        self.id
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        for p in &self.parsers {
            match p.parse(probe)? {
                Some(doc) => return Ok(Some(doc)),
                None => continue,
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysDecline;
    impl Parser for AlwaysDecline {
        fn id(&self) -> &'static str {
            "decline"
        }
        fn parse(&self, _: &Probe) -> Result<Option<ParsedDocument>, Error> {
            Ok(None)
        }
    }

    struct AlwaysMatch {
        tag: &'static str,
    }
    impl Parser for AlwaysMatch {
        fn id(&self) -> &'static str {
            self.tag
        }
        fn parse(&self, _: &Probe) -> Result<Option<ParsedDocument>, Error> {
            Ok(Some(ParsedDocument {
                text: self.tag.to_string(),
                ..Default::default()
            }))
        }
    }

    struct AlwaysErr;
    impl Parser for AlwaysErr {
        fn id(&self) -> &'static str {
            "err"
        }
        fn parse(&self, _: &Probe) -> Result<Option<ParsedDocument>, Error> {
            Err(Error::UnsupportedFormat {
                format: "test-err".to_string(),
            })
        }
    }

    fn probe_bytes(bytes: &[u8]) -> Probe<'_> {
        Probe::new(bytes, None, None)
    }

    #[test]
    fn decline_returns_ok_none() {
        let p = AlwaysDecline;
        let probe = probe_bytes(b"hello");
        assert!(p.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn chain_empty_returns_none() {
        let chain = ChainParser::new("chain", vec![]);
        assert!(chain.parse(&probe_bytes(b"data")).unwrap().is_none());
    }

    #[test]
    fn chain_first_match_wins() {
        let chain = ChainParser::new(
            "chain",
            vec![
                Box::new(AlwaysMatch { tag: "first" }),
                Box::new(AlwaysMatch { tag: "second" }),
            ],
        );
        let doc = chain.parse(&probe_bytes(b"data")).unwrap().unwrap();
        assert_eq!(doc.text, "first");
    }

    #[test]
    fn chain_skips_declines_to_find_match() {
        let chain = ChainParser::new(
            "chain",
            vec![
                Box::new(AlwaysDecline),
                Box::new(AlwaysMatch { tag: "found" }),
            ],
        );
        let doc = chain.parse(&probe_bytes(b"data")).unwrap().unwrap();
        assert_eq!(doc.text, "found");
    }

    #[test]
    fn chain_all_decline_returns_none() {
        let chain = ChainParser::new(
            "chain",
            vec![Box::new(AlwaysDecline), Box::new(AlwaysDecline)],
        );
        assert!(chain.parse(&probe_bytes(b"data")).unwrap().is_none());
    }

    #[test]
    fn chain_err_short_circuits() {
        let chain = ChainParser::new(
            "chain",
            vec![
                Box::new(AlwaysErr),
                Box::new(AlwaysMatch { tag: "unreachable" }),
            ],
        );
        assert!(chain.parse(&probe_bytes(b"data")).is_err());
    }

    #[test]
    fn probe_header_truncates_to_limit() {
        let bytes: Vec<u8> = (0u8..255).cycle().take(PROBE_HEADER_LEN + 100).collect();
        let probe = Probe::new(&bytes, None, None);
        assert_eq!(probe.header().len(), PROBE_HEADER_LEN);
        assert_eq!(probe.bytes().len(), PROBE_HEADER_LEN + 100);
    }

    #[test]
    fn probe_header_short_input() {
        let probe = Probe::new(b"short", None, None);
        assert_eq!(probe.header().len(), 5);
        assert_eq!(probe.header(), b"short");
    }

    #[test]
    fn probe_extension_normal() {
        let probe = Probe::new(b"", Some("doc.md"), None);
        assert_eq!(probe.extension(), Some("md"));
    }

    #[test]
    fn probe_extension_path_with_dir() {
        let probe = Probe::new(b"", Some("/home/user/notes/README.md"), None);
        assert_eq!(probe.extension(), Some("md"));
    }

    #[test]
    fn probe_extension_no_ext() {
        let probe = Probe::new(b"", Some("Makefile"), None);
        assert_eq!(probe.extension(), None);
    }

    #[test]
    fn probe_extension_hidden_file() {
        let probe = Probe::new(b"", Some(".gitignore"), None);
        assert_eq!(probe.extension(), None);
    }

    #[test]
    fn probe_extension_none_path() {
        let probe = Probe::new(b"", None, None);
        assert_eq!(probe.extension(), None);
    }

    #[test]
    fn dublin_core_default_is_empty() {
        let dc = DocumentMetadata::default();
        assert!(dc.title.is_none());
        assert!(dc.creator.is_empty());
        assert!(dc.subject.is_empty());
        assert!(dc.description.is_none());
        assert!(dc.relation.is_empty());
    }

    #[test]
    fn dublin_core_roundtrips_json() {
        let dc = DocumentMetadata {
            title: Some("Test Document".to_string()),
            creator: vec!["Alice".to_string(), "Bob".to_string()],
            date: Some("2026-06-13".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&dc).unwrap();
        let dc2: DocumentMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(dc, dc2);
    }

    #[test]
    fn parsed_document_default() {
        let doc = ParsedDocument::default();
        assert!(doc.text.is_empty());
        assert!(doc.blocks.is_empty());
        assert!(doc.title.is_none());
        assert_eq!(doc.metadata, DocumentMetadata::default());
    }

    #[test]
    fn chain_parser_is_itself_a_parser() {
        let inner = ChainParser::new("inner", vec![Box::new(AlwaysMatch { tag: "nested" })]);
        let outer = ChainParser::new("outer", vec![Box::new(inner)]);
        let doc = outer.parse(&probe_bytes(b"data")).unwrap().unwrap();
        assert_eq!(doc.text, "nested");
    }
}
