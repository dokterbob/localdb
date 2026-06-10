//! Format detection and extraction crate for localdb.
//!
//! Converts Markdown, plain text, HTML, and text-layer PDF to normalized
//! document text and [`Block`] structures.
//!
//! Entry points:
//! - [`detect_format`] — determine [`Format`] from filename/bytes.
//! - [`extract`] — extract normalized text + blocks from raw bytes.
//!
//! Blocks are intermediate structural units (headings, paragraphs, code fences,
//! lists) used by the chunker. Spans in every block index into the returned
//! normalized text string exactly.

pub mod detect;
pub mod html;
pub mod markdown;
pub mod pdf;
pub mod plaintext;

use localdb_core::{Block, Error, Span};

/// The output of a successful extraction.
#[derive(Debug, Clone)]
pub struct ExtractionOutput {
    /// The normalized document text. All block spans index into this.
    pub text: String,
    /// Structural blocks in document order.
    pub blocks: Vec<Block>,
    /// Optional title detected during extraction.
    pub title: Option<String>,
}

/// Detected document format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    PlainText,
    Html,
    Pdf,
}

/// Detect the format of a document from its filename hint and/or raw bytes.
///
/// `filename` is optional. When both are available, magic-byte detection
/// takes precedence (so a `.txt` file that starts with `%PDF-` is treated as PDF).
pub fn detect_format(filename: Option<&str>, bytes: &[u8]) -> Result<Format, Error> {
    detect::detect_format(filename, bytes)
}

/// Extract normalized text and blocks from raw bytes.
///
/// `filename` is used for format detection if provided.
///
/// # Errors
/// Returns [`Error::UnsupportedFormat`] for formats not in the v1 matrix
/// (e.g. scanned PDF with no text layer, DOCX, etc.).
pub fn extract(bytes: &[u8], filename: Option<&str>) -> Result<ExtractionOutput, Error> {
    let format = detect_format(filename, bytes)?;
    extract_with_format(bytes, format)
}

/// Extract with an already-detected format.
pub fn extract_with_format(bytes: &[u8], format: Format) -> Result<ExtractionOutput, Error> {
    match format {
        Format::Markdown => {
            let text = std::str::from_utf8(bytes).map_err(|e| Error::InvalidRequest {
                message: format!("Markdown is not valid UTF-8: {e}"),
            })?;
            markdown::extract_markdown(text)
        }
        Format::PlainText => {
            let text = std::str::from_utf8(bytes).map_err(|e| Error::InvalidRequest {
                message: format!("Plain text is not valid UTF-8: {e}"),
            })?;
            plaintext::extract_plaintext(text)
        }
        Format::Html => {
            let text = std::str::from_utf8(bytes).map_err(|e| Error::InvalidRequest {
                message: format!("HTML is not valid UTF-8: {e}"),
            })?;
            html::extract_html(text)
        }
        Format::Pdf => pdf::extract_pdf(bytes),
    }
}

/// Build a Block (without document_id filled in — caller sets it).
///
/// Helper used by extractor implementations. The `document_id` field is set to
/// an empty string; the ingestion pipeline fills it in before persisting.
pub(crate) fn make_block(
    ordinal: usize,
    kind: localdb_core::BlockKind,
    text: String,
    span: Span,
    heading_path: Vec<String>,
) -> Block {
    Block {
        // document_id is filled in by the caller / ingestion pipeline
        document_id: String::new(),
        ordinal,
        kind,
        text,
        span,
        heading_path,
    }
}
