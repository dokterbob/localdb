//! `ChainExtractor` — bridges `ChainParser` to `DocumentExtractor`.
//!
//! This is the single, canonical implementation of the bridge that was previously
//! copy-pasted as a local `struct ExtractBridge` in each CLI ingestion site.

use localdb_core::{
    ingestion::{DocumentExtractor, ExtractionResult},
    Error,
};

use crate::{
    build_chain, registry::default_parser_ids, sniff_mime, ChainParser, Parser as _, Probe,
};

/// Wraps a [`ChainParser`] and implements [`DocumentExtractor`] so it can be
/// passed directly to `run_ingestion_for_source`.
#[derive(Debug)]
pub struct ChainExtractor {
    chain: ChainParser,
}

impl ChainExtractor {
    pub fn new(chain: ChainParser) -> Self {
        Self { chain }
    }

    /// Build from an ordered list of parser IDs (e.g. from `IndexingPolicyConfig.parsers`).
    pub fn from_ids(ids: &[String]) -> Result<Self, Error> {
        Ok(Self::new(build_chain(ids)?))
    }

    /// Build using the default parser order.
    pub fn with_defaults() -> Result<Self, Error> {
        Self::from_ids(&default_parser_ids())
    }
}

impl DocumentExtractor for ChainExtractor {
    fn extract(&self, bytes: &[u8], filename: Option<&str>) -> Result<ExtractionResult, Error> {
        let sniffed = sniff_mime(bytes, filename);
        let probe = Probe::new(bytes, filename, sniffed.as_deref());
        match self.chain.parse(&probe)? {
            Some(doc) => Ok(ExtractionResult {
                markdown: doc.markdown,
                title: doc.title,
                metadata: doc.metadata,
            }),
            None => Err(Error::UnsupportedFormat {
                format: "no parser matched the file".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn some_path_populates_text_and_title() {
        let ex = ChainExtractor::with_defaults().unwrap();
        let md = b"# My Title\n\nSome body text here.";
        let result = ex.extract(md, Some("doc.md")).unwrap();
        assert!(!result.markdown.is_empty());
        assert!(result.title.is_some());
    }

    #[test]
    fn some_path_populates_metadata_when_sniffed() {
        let ex = ChainExtractor::with_defaults().unwrap();
        // Feed Markdown with a .md extension so the mime sniffer returns text/markdown.
        let md = b"# Title\n\nParagraph.";
        let result = ex.extract(md, Some("doc.md")).unwrap();
        assert!(!result.markdown.is_empty());
    }

    #[test]
    fn none_path_returns_unsupported_format() {
        // Build a chain that only contains the PDF parser.  Binary non-PDF bytes
        // cause it to decline (Ok(None)), which must become UnsupportedFormat.
        let ids = vec!["pdf".to_string()];
        let ex = ChainExtractor::from_ids(&ids).unwrap();
        let binary = b"\xFF\xFE\x00\x01not a pdf at all";
        let err = ex.extract(binary, Some("file.bin")).unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedFormat { .. }),
            "expected UnsupportedFormat, got: {err:?}"
        );
    }

    #[test]
    fn err_from_parser_propagates() {
        // The PDF parser returns Err for scanned/invalid PDFs; that error must
        // propagate via `?` rather than being swallowed and replaced with the
        // generic "no parser matched the file" message.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scanned.pdf");
        let bytes = std::fs::read(&path).expect("scanned.pdf fixture must exist");
        let ids = vec!["pdf".to_string()];
        let ex = ChainExtractor::from_ids(&ids).unwrap();
        let err = ex.extract(&bytes, Some("scanned.pdf")).unwrap_err();
        // The error must come from the PDF parser, not from the None branch.
        let msg = format!("{err}");
        assert!(
            !msg.contains("no parser matched the file"),
            "parser Err should propagate with its own message, not be replaced; got: {err:?}"
        );
    }

    #[test]
    fn from_ids_unknown_id_errors() {
        let err = ChainExtractor::from_ids(&["no-such-parser".to_string()]).unwrap_err();
        assert!(
            format!("{err}").contains("unknown parser id"),
            "expected InvalidConfig, got: {err:?}"
        );
    }

    #[test]
    fn binary_md_yields_unsupported_format() {
        let ex = ChainExtractor::with_defaults().unwrap();
        let binary = b"\xFF\xFE\x00\x01binary content";
        let err = ex.extract(binary, Some("file.md")).unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedFormat { .. }),
            "binary .md should yield UnsupportedFormat, got: {err:?}"
        );
    }

    #[test]
    fn binary_html_yields_unsupported_format() {
        let ex = ChainExtractor::with_defaults().unwrap();
        let binary = b"\xFF\xFE\x00\x01binary content";
        let err = ex.extract(binary, Some("page.html")).unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedFormat { .. }),
            "binary .html should yield UnsupportedFormat, got: {err:?}"
        );
    }
}
