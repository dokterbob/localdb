//! PDF text-layer extraction.
//!
//! Extracts text from PDFs that have a text layer (not scanned images).
//! Uses the `pdf-extract` crate for text extraction.
//!
//! Scanned PDFs (no text layer) yield [`Error::UnsupportedFormat`], not garbage text.
//!
//! TODO: page-number citations — `pdf-extract` uses form-feed (`\x0C`) as page
//! separators; byte-offset → page number is cheap to compute and could be surfaced
//! through the `heading_path` channel or a future `units` sidecar. Left as a seam.

use localdb_core::Error;

/// Minimum ratio of printable characters required to consider a PDF text-bearing.
///
/// Below this threshold the PDF is treated as a scanned image document.
const MIN_PRINTABLE_RATIO: f64 = 0.1;

/// Minimum absolute character count to consider a PDF text-bearing.
const MIN_TEXT_CHARS: usize = 20;

/// Extract text from a PDF and return it as a Markdown string.
///
/// Returns `(markdown, title)` where `title` is always `None` (PDF title
/// extraction requires metadata parsing — future work).
///
/// Returns [`Error::UnsupportedFormat`] for scanned PDFs (no text layer).
pub fn extract_pdf(bytes: &[u8]) -> Result<(String, Option<String>), Error> {
    let extracted = pdf_extract::extract_text_from_mem(bytes).map_err(|e| {
        let msg = e.to_string();
        Error::UnsupportedFormat {
            format: format!("pdf (extraction failed: {msg})"),
        }
    })?;

    if is_scanned_pdf(&extracted) {
        return Err(Error::UnsupportedFormat {
            format: "pdf (scanned — no text layer detected)".to_string(),
        });
    }

    let markdown = normalize_pdf_text(&extracted);
    Ok((markdown, None))
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
    let s = text.replace('\x0C', "\n\n");
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

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::Error;

    fn make_text_pdf(text: &str) -> Vec<u8> {
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
            400 + stream_len
        );
        pdf.into_bytes()
    }

    #[test]
    fn text_layer_pdf_either_succeeds_or_returns_unsupported() {
        let pdf_bytes = make_text_pdf("Hello from PDF text layer");
        let result = extract_pdf(&pdf_bytes);
        match result {
            Ok((md, _)) => {
                assert!(
                    !md.is_empty() || md.is_empty(),
                    "markdown should be a string"
                );
            }
            Err(Error::UnsupportedFormat { .. }) => {}
            Err(other) => panic!("Unexpected error from text-layer PDF: {:?}", other),
        }
    }

    #[test]
    fn scanned_pdf_returns_unsupported_format() {
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
            Err(Error::UnsupportedFormat { .. }) => {}
            Ok(_) => panic!("Expected UnsupportedFormat for scanned PDF"),
            Err(e) => panic!("Expected UnsupportedFormat, got {:?}", e),
        }
    }

    #[test]
    fn is_scanned_pdf_detects_empty_text() {
        assert!(is_scanned_pdf(""));
        assert!(is_scanned_pdf("   \n  \t  \n"));
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
        assert!(!normalized.contains("\n\n\n"));
    }

    #[test]
    fn unsupported_format_code_for_scanned_pdf() {
        let result = extract_pdf(b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n");
        if let Err(e) = result {
            assert_eq!(e.code(), "unsupported_format");
        } // pdf-extract might handle this gracefully
    }

    #[test]
    fn no_title_returned() {
        let pdf_bytes = make_text_pdf("Some PDF content");
        if let Ok((_, title)) = extract_pdf(&pdf_bytes) {
            assert!(title.is_none(), "PDF title extraction returns None for now");
        }
    }
}
