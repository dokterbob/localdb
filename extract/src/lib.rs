//! Format detection and extraction crate for localdb.
//!
//! The primary API is the extensible parser chain:
//! - [`registry::build_chain`] — build a `ChainParser` from an ordered list of IDs.
//! - [`registry::default_parser_ids`] — the default parser order.
//! - [`sniff_mime`] — advisory MIME sniffing (magic bytes + extension).
//!
//! Re-exports from `localdb_core`:
//! - [`ChainParser`], [`Parser`], [`Probe`], [`ParsedDocument`] — core chain types.

pub mod chain_extractor;
pub mod html;
pub mod markdown;
pub mod mime;
pub mod parsers;
pub mod pdf;
pub mod plaintext;
pub mod registry;

// Re-export chain types for consumers that wire ExtractBridge.
pub use chain_extractor::ChainExtractor;
pub use localdb_core::parser::{ChainParser, DocumentMetadata, ParsedDocument, Parser, Probe};
pub use mime::sniff_mime;
pub use registry::{build_chain, default_parser_ids};

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::ingestion::DocumentExtractor as _;

    #[test]
    fn chain_extractor_unsupported_format_on_binary() {
        // Non-UTF-8 binary with no recognisable magic bytes declines every
        // parser in the default chain, hitting the `None => UnsupportedFormat` arm.
        use localdb_core::Error;
        let ex = ChainExtractor::with_defaults().unwrap();
        let binary = b"\xFF\xFE\x00\x01some binary garbage that is not utf-8";
        let err = ex.extract(binary, None).unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedFormat { .. }),
            "expected UnsupportedFormat, got: {err:?}"
        );
    }
}
