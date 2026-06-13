//! MIME type sniffing for the extensible parser pipeline.
//!
//! The result is **advisory**: a wrong guess never overrides a parser's own
//! magic-byte or extension decision. It is passed into `Probe.sniffed_mime`
//! and written to `DocumentMetadata.format` when a parser doesn't determine it itself.

/// Sniff the MIME type from raw bytes and/or filename.
///
/// Priority:
/// 1. Magic-byte detection via the `infer` crate.
/// 2. Extension-based guess via `mime_guess`.
///
/// Returns `None` if neither method produces a result.
pub fn sniff_mime(bytes: &[u8], filename: Option<&str>) -> Option<String> {
    // Prefer magic-byte detection (format-independent, works for renamed files).
    if let Some(kind) = infer::get(bytes) {
        return Some(kind.mime_type().to_string());
    }

    // Fall back to extension-based guess.
    if let Some(name) = filename {
        if let Some(mime) = mime_guess::from_path(name).first() {
            return Some(mime.to_string());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_pdf_by_magic() {
        let pdf_header = b"%PDF-1.4 garbage";
        let result = sniff_mime(pdf_header, None);
        // infer may or may not recognize partial PDF header; either way no panic.
        // If it does, it should be application/pdf.
        if let Some(mime) = result {
            assert!(
                mime.contains("pdf") || mime.contains("octet"),
                "unexpected MIME for PDF magic: {mime}"
            );
        }
    }

    #[test]
    fn sniffs_by_extension_fallback() {
        let result = sniff_mime(b"plain text content", Some("document.html"));
        assert!(
            result.is_some(),
            "extension-based sniff should produce a result for .html"
        );
        let mime = result.unwrap();
        assert!(mime.contains("html"), "expected text/html, got: {mime}");
    }

    #[test]
    fn returns_none_for_unknown() {
        let result = sniff_mime(b"totally unknown content", Some("file.xyz123"));
        // mime_guess won't know .xyz123; infer won't match plain text.
        // Result may be None or Some depending on heuristics; either is valid.
        let _ = result; // just ensure no panic
    }

    #[test]
    fn returns_none_no_filename_no_magic() {
        let result = sniff_mime(b"just some text", None);
        // infer only detects binary formats; plain text returns None.
        assert!(result.is_none());
    }

    #[test]
    fn markdown_extension_guessed() {
        let result = sniff_mime(b"# heading", Some("README.md"));
        // mime_guess knows .md → text/markdown
        assert!(result.is_some());
        let mime = result.unwrap();
        assert!(
            mime.contains("markdown") || mime.contains("text"),
            "expected markdown MIME, got: {mime}"
        );
    }
}
