//! Plain text extraction: CRLF-normalize and return as Markdown.
//!
//! Plain text has no Markdown structure — we return it as-is so that
//! `MarkdownSplitter` treats it as unstructured prose (splitting on blank lines).
//! We deliberately do NOT escape Markdown metacharacters: a stray `#` is harmless
//! and escaping would corrupt citation snippets.

use localdb_core::Error;

/// Extract a plain text document.
///
/// Returns `(markdown, title)` where `markdown` is the CRLF-normalized input and
/// `title` is always `None` (plain text has no title structure).
pub fn extract_plaintext(input: &str) -> Result<(String, Option<String>), Error> {
    let markdown = input.replace("\r\n", "\n").replace('\r', "\n");
    Ok((markdown, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_preserves_text() {
        let input = "Hello world. This is plain text.";
        let (out, _) = extract_plaintext(input).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn no_title_for_plain_text() {
        let input = "Some text.";
        let (_, title) = extract_plaintext(input).unwrap();
        assert!(title.is_none());
    }

    #[test]
    fn empty_input_returns_empty_string() {
        let (out, _) = extract_plaintext("").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn crlf_normalized_to_lf() {
        let input = "Line one.\r\nLine two.\r\n";
        let (out, _) = extract_plaintext(input).unwrap();
        assert!(!out.contains('\r'), "CRLF should be normalized to LF");
        assert!(out.contains("Line one.\nLine two."));
    }

    #[test]
    fn cr_only_normalized_to_lf() {
        let input = "Line one.\rLine two.\r";
        let (out, _) = extract_plaintext(input).unwrap();
        assert!(!out.contains('\r'));
    }

    #[test]
    fn stray_hash_not_escaped() {
        // A plain-text file with a # symbol should NOT have it escaped —
        // MarkdownSplitter will treat it as a heading, which is acceptable.
        let input = "# not a heading really\n\nSome content.\n";
        let (out, _) = extract_plaintext(input).unwrap();
        assert!(out.contains("# not a heading"), "# must not be escaped");
    }
}
