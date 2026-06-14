//! Format detection and extraction crate for localdb.
//!
//! The primary API is the extensible parser chain:
//! - [`registry::build_chain`] — build a `ChainParser` from an ordered list of IDs.
//! - [`registry::default_parser_ids`] — the default parser order.
//! - [`sniff_mime`] — advisory MIME sniffing (magic bytes + extension).
//!
//! Re-exports from `localdb_core`:
//! - [`ChainParser`], [`Parser`], [`Probe`], [`ParsedDocument`] — core chain types.
//!
//! Compatibility shims kept for one release so `golden_tests.rs` and any other
//! direct callers of the old API keep compiling without change:
//! - [`extract`] — thin shim over a default chain.
//! - [`detect_format`] — delegates to `detect::detect_format`.
//! - [`ExtractionOutput`] — the old output type.
//! - [`Format`] — the old format enum.

pub mod detect;
pub mod html;
pub mod markdown;
pub mod mime;
pub mod parsers;
pub mod pdf;
pub mod plaintext;
pub mod registry;

use localdb_core::{Block, BlockKind, Error, Span};

// Re-export chain types for consumers that wire ExtractBridge.
pub use localdb_core::parser::{ChainParser, DocumentMetadata, ParsedDocument, Parser, Probe};
pub use mime::sniff_mime;
pub use registry::{build_chain, default_parser_ids};

// ---------------------------------------------------------------------------
// Legacy output types (kept for shims and tests)
// ---------------------------------------------------------------------------

/// The output of a successful extraction (legacy type; prefer `ParsedDocument`).
#[derive(Debug, Clone)]
pub struct ExtractionOutput {
    /// The normalized document text. All block spans index into this.
    pub text: String,
    /// Structural blocks in document order.
    pub blocks: Vec<Block>,
    /// Optional title detected during extraction.
    pub title: Option<String>,
}

/// Detected document format (legacy enum; prefer the parser chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    PlainText,
    Html,
    Pdf,
}

// ---------------------------------------------------------------------------
// Deprecated shims
// ---------------------------------------------------------------------------

/// Detect the format of a document from its filename hint and/or raw bytes.
///
/// When both are available, magic-byte detection takes precedence.
pub fn detect_format(filename: Option<&str>, bytes: &[u8]) -> Result<Format, Error> {
    detect::detect_format(filename, bytes)
}

/// Extract normalized text and blocks from raw bytes using the default parser chain.
///
/// `filename` is used for MIME sniffing and extension-based parser selection.
///
/// # Errors
/// Returns [`Error::UnsupportedFormat`] when no parser in the default chain
/// accepts the input (e.g. scanned PDF with no text layer, binary file).
pub fn extract(bytes: &[u8], filename: Option<&str>) -> Result<ExtractionOutput, Error> {
    let sniffed = sniff_mime(bytes, filename);
    let probe = Probe::new(bytes, filename, sniffed.as_deref());
    let chain = build_chain(&default_parser_ids()).expect("default parser IDs are always valid");
    match chain.parse(&probe)? {
        Some(doc) => Ok(ExtractionOutput {
            text: doc.text,
            blocks: doc.blocks,
            title: doc.title,
        }),
        None => Err(Error::UnsupportedFormat {
            format: "no parser matched the file".to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build a Block (without document_id filled in — caller sets it).
///
/// Helper used by extractor implementations. The `document_id` field is set to
/// an empty string; the ingestion pipeline fills it in before persisting.
pub(crate) fn make_block(
    ordinal: usize,
    kind: BlockKind,
    text: String,
    span: Span,
    heading_path: Vec<String>,
) -> Block {
    Block {
        document_id: String::new(),
        ordinal,
        kind,
        text,
        span,
        heading_path,
    }
}
