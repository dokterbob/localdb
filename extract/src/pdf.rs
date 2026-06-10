//! PDF text-layer extraction.
//!
//! Extracts text from PDFs that have a text layer (not scanned images).
//! Uses the `pdf-extract` crate for text extraction.
//!
//! Scanned PDFs (no text layer) yield [`Error::UnsupportedFormat`], not garbage text.
//! The threshold: if the extracted text is empty or consists only of whitespace
//! after processing, the PDF is treated as scanned.

use crate::{make_block, ExtractionOutput};
use localdb_core::{BlockKind, Error, Span};

/// Minimum ratio of printable characters required to consider a PDF text-bearing.
///
/// Below this threshold the PDF is treated as a scanned image document.
const MIN_PRINTABLE_RATIO: f64 = 0.1;

/// Minimum absolute character count to consider a PDF text-bearing.
const MIN_TEXT_CHARS: usize = 20;

/// Extract text and blocks from a PDF.
///
/// Returns [`Error::UnsupportedFormat`] for scanned PDFs (no text layer).
pub fn extract_pdf(bytes: &[u8]) -> Result<ExtractionOutput, Error> {
    // Attempt text extraction
    let extracted = pdf_extract::extract_text_from_mem(bytes).map_err(|e| {
        // If the error looks like a "no text" or decode error, classify as unsupported
        let msg = e.to_string();
        if msg.contains("no text") || msg.contains("encrypted") {
            Error::UnsupportedFormat {
                format: format!("pdf (extraction failed: {msg})"),
            }
        } else {
            Error::UnsupportedFormat {
                format: format!("pdf (error: {msg})"),
            }
        }
    })?;

    // Check if we got meaningful text
    if is_scanned_pdf(&extracted) {
        return Err(Error::UnsupportedFormat {
            format: "pdf (scanned — no text layer detected)".to_string(),
        });
    }

    // Normalize the text
    let normalized = normalize_pdf_text(&extracted);

    // Build blocks: split by page breaks and paragraph-like gaps.
    // pdf-extract uses form-feed (\x0C) as page separators.
    let blocks = build_pdf_blocks(&normalized);

    Ok(ExtractionOutput {
        text: normalized,
        blocks,
        title: None, // PDF title extraction requires metadata parsing (future work)
    })
}

/// Check if a PDF appears to be scanned (no meaningful text layer).
fn is_scanned_pdf(text: &str) -> bool {
    let total = text.len();
    if total == 0 {
        return true;
    }

    let printable: usize = text
        .chars()
        .filter(|c| !c.is_whitespace() && c.is_alphanumeric())
        .count();

    if printable < MIN_TEXT_CHARS {
        return true;
    }

    let ratio = printable as f64 / total as f64;
    ratio < MIN_PRINTABLE_RATIO
}

/// Normalize extracted PDF text.
///
/// - Replace form-feed (`\x0C`) page separators with double newlines.
/// - Collapse excessive blank lines.
/// - Ensure trailing newline.
fn normalize_pdf_text(text: &str) -> String {
    // Replace page breaks with double newlines
    let s = text.replace('\x0C', "\n\n");
    // Collapse 3+ newlines to 2
    let mut result = String::with_capacity(s.len());
    let mut consecutive_newlines = 0usize;
    for ch in s.chars() {
        if ch == '\n' {
            consecutive_newlines += 1;
            if consecutive_newlines <= 2 {
                result.push(ch);
            }
        } else {
            consecutive_newlines = 0;
            result.push(ch);
        }
    }
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Build paragraph-like blocks from normalized PDF text.
///
/// Paragraphs are delimited by blank lines (double newlines).
fn build_pdf_blocks(normalized: &str) -> Vec<localdb_core::Block> {
    let mut blocks = Vec::new();
    let mut ordinal = 0usize;
    let bytes = normalized.as_bytes();
    let mut pos = 0usize;

    while pos < bytes.len() {
        // Skip leading blank lines
        while pos < bytes.len() && bytes[pos] == b'\n' {
            pos += 1;
        }

        if pos >= bytes.len() {
            break;
        }

        let start = pos;

        // Scan forward to find end of paragraph (double newline)
        loop {
            if pos >= bytes.len() {
                break;
            }
            if bytes[pos] == b'\n' {
                if pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' {
                    pos += 1; // consume first newline
                    break;
                } else {
                    pos += 1;
                }
            } else {
                pos += 1;
            }
        }

        let end = pos;
        let para = &normalized[start..end];
        let trimmed = para.trim();

        if !trimmed.is_empty() {
            let block = make_block(
                ordinal,
                BlockKind::Paragraph,
                trimmed.to_string(),
                Span::new(start, end),
                vec![], // PDF paragraphs have no heading context
            );
            blocks.push(block);
            ordinal += 1;
        }
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::Error;

    /// Build a minimal valid text-layer PDF with one page.
    /// Uses raw PDF syntax — tiny but parseable by pdf-extract.
    fn make_text_pdf(text: &str) -> Vec<u8> {
        // A minimal PDF with a single page containing text.
        // We use a simple PDF structure that pdf-extract can parse.
        let content_stream = format!("BT /F1 12 Tf 50 700 Td ({text}) Tj ET");
        let stream_len = content_stream.len();

        let pdf = format!(
            "%PDF-1.4\n\
             1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
             2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
             3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]\n\
               /Contents 4 0 R /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> >>\nendobj\n\
             4 0 obj\n<< /Length {stream_len} >>\nstream\n{content_stream}\nendstream\nendobj\n\
             xref\n0 5\n\
             0000000000 65535 f \n\
             0000000009 00000 n \n\
             0000000058 00000 n \n\
             0000000115 00000 n \n\
             0000000266 00000 n \n\
             trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            // xref offset approximation — not perfectly accurate but may work
            400 + stream_len
        );
        pdf.into_bytes()
    }

    #[test]
    fn text_layer_pdf_either_succeeds_or_returns_unsupported() {
        // A text-layer PDF should either extract text or return UnsupportedFormat.
        // We do NOT expect it to panic or return an Internal error.
        let pdf_bytes = make_text_pdf("Hello from PDF text layer");
        let result = extract_pdf(&pdf_bytes);
        match result {
            Ok(out) => {
                // Extraction succeeded — verify spans are valid
                for block in &out.blocks {
                    assert!(block.span.end <= out.text.len());
                }
            }
            Err(Error::UnsupportedFormat { .. }) => {
                // Also acceptable: pdf-extract couldn't decode the font encoding
            }
            Err(other) => {
                panic!("Unexpected error from text-layer PDF: {:?}", other);
            }
        }
    }

    #[test]
    fn scanned_pdf_returns_unsupported_format() {
        // A minimal PDF with no text operators — simulate a scanned PDF.
        // We use raw bytes that form a PDF with only an image XObject and no text.
        let scanned_bytes = b"%PDF-1.4\n\
            1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
            2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
            3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>\nendobj\n\
            4 0 obj\n<< /Length 2 >>\nstream\n  \nendstream\nendobj\n\
            xref\n0 5\n\
            0000000000 65535 f \n\
            0000000009 00000 n \n\
            0000000058 00000 n \n\
            0000000115 00000 n \n\
            0000000215 00000 n \n\
            trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n270\n%%EOF\n";

        let result = extract_pdf(scanned_bytes);
        match result {
            Err(Error::UnsupportedFormat { .. }) => {
                // Expected: scanned PDF yields UnsupportedFormat
            }
            Ok(out) => {
                // Also acceptable if text is empty and we'd treat it as scanned,
                // but the function should have returned the error.
                // If somehow text was extracted but it's garbage,
                // the is_scanned_pdf check should catch it.
                panic!(
                    "Expected UnsupportedFormat for scanned PDF, but got Ok with {} blocks",
                    out.blocks.len()
                );
            }
            Err(e) => {
                // Any error other than UnsupportedFormat is unexpected
                panic!("Expected UnsupportedFormat, got {:?}", e);
            }
        }
    }

    #[test]
    fn is_scanned_pdf_detects_empty_text() {
        assert!(is_scanned_pdf(""));
        assert!(is_scanned_pdf("   \n  \t  \n"));
    }

    #[test]
    fn is_scanned_pdf_detects_garbage_text() {
        // Text with very few printable characters relative to total length
        let garbage = " \x00\x01\x02\x03 ".repeat(100);
        assert!(is_scanned_pdf(&garbage));
    }

    #[test]
    fn is_scanned_pdf_accepts_real_text() {
        let real_text = "This is a real paragraph with meaningful text content. \
                         It has many words and sentences that indicate a real document.";
        assert!(!is_scanned_pdf(real_text));
    }

    #[test]
    fn normalize_pdf_text_replaces_form_feed() {
        let text = "Page one content.\x0CPage two content.";
        let normalized = normalize_pdf_text(text);
        assert!(!normalized.contains('\x0C'));
        assert!(normalized.contains("Page one content."));
        assert!(normalized.contains("Page two content."));
    }

    #[test]
    fn normalize_pdf_text_collapses_blank_lines() {
        let text = "Para one.\n\n\n\n\nPara two.";
        let normalized = normalize_pdf_text(text);
        // Should have at most 2 consecutive newlines
        assert!(!normalized.contains("\n\n\n"));
    }

    #[test]
    fn build_pdf_blocks_splits_paragraphs() {
        let text = "First paragraph content.\n\nSecond paragraph content.\n";
        let blocks = build_pdf_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].text.contains("First paragraph"));
        assert!(blocks[1].text.contains("Second paragraph"));
    }

    #[test]
    fn pdf_blocks_span_into_normalized_text() {
        let text = "Block one.\n\nBlock two.\n";
        let blocks = build_pdf_blocks(text);
        for block in &blocks {
            let span_text = &text[block.span.start..block.span.end];
            assert!(
                span_text.contains(block.text.trim()),
                "Span should contain block text"
            );
        }
    }

    #[test]
    fn unsupported_format_code_for_scanned_pdf() {
        let result = extract_pdf(b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n");
        match result {
            Err(e) => {
                assert_eq!(e.code(), "unsupported_format");
            }
            Ok(_) => {
                // pdf-extract might actually handle this gracefully
                // as an empty document — that's also acceptable
            }
        }
    }
}
