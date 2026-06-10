//! Format detection from filename hints and magic bytes.

use crate::Format;
use localdb_core::Error;

/// PDF magic bytes: `%PDF-`
const PDF_MAGIC: &[u8] = b"%PDF-";

/// Detect document format from optional filename and raw bytes.
///
/// Priority:
/// 1. If the bytes start with PDF magic (`%PDF-`), return `Format::Pdf`.
/// 2. Otherwise fall back to the filename extension.
/// 3. If neither gives a clear answer, default to `PlainText`.
pub fn detect_format(filename: Option<&str>, bytes: &[u8]) -> Result<Format, Error> {
    // Magic-byte detection takes precedence
    if bytes.starts_with(PDF_MAGIC) {
        return Ok(Format::Pdf);
    }

    if let Some(name) = filename {
        let lower = name.to_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");

        match ext {
            "md" | "markdown" | "mdown" | "mkd" | "mkdn" => return Ok(Format::Markdown),
            "html" | "htm" | "xhtml" => return Ok(Format::Html),
            "pdf" => return Ok(Format::Pdf),
            "txt" | "text" => return Ok(Format::PlainText),
            _ => {}
        }
    }

    // Heuristic: if the content looks like HTML (starts with `<`), treat as HTML
    let trimmed = bytes.iter().position(|&b| !b.is_ascii_whitespace());
    if let Some(pos) = trimmed {
        if bytes[pos..].starts_with(b"<") {
            return Ok(Format::Html);
        }
    }

    // Default to plain text
    Ok(Format::PlainText)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_markdown_by_extension() {
        assert_eq!(
            detect_format(Some("readme.md"), b"# Hello").unwrap(),
            Format::Markdown
        );
        assert_eq!(
            detect_format(Some("file.markdown"), b"# Hello").unwrap(),
            Format::Markdown
        );
    }

    #[test]
    fn detects_html_by_extension() {
        assert_eq!(
            detect_format(Some("page.html"), b"<html>").unwrap(),
            Format::Html
        );
        assert_eq!(
            detect_format(Some("page.htm"), b"<html>").unwrap(),
            Format::Html
        );
    }

    #[test]
    fn detects_plaintext_by_extension() {
        assert_eq!(
            detect_format(Some("notes.txt"), b"hello world").unwrap(),
            Format::PlainText
        );
    }

    #[test]
    fn detects_pdf_by_magic_bytes() {
        assert_eq!(
            detect_format(Some("doc.pdf"), b"%PDF-1.4 %...").unwrap(),
            Format::Pdf
        );
        // Magic takes precedence over extension
        assert_eq!(
            detect_format(Some("tricky.txt"), b"%PDF-1.7 %...").unwrap(),
            Format::Pdf
        );
    }

    #[test]
    fn detects_pdf_by_extension_when_no_magic() {
        // A file named .pdf but without magic bytes still counts as PDF
        assert_eq!(
            detect_format(Some("doc.pdf"), b"not a real pdf").unwrap(),
            Format::Pdf
        );
    }

    #[test]
    fn html_heuristic_no_filename() {
        assert_eq!(
            detect_format(None, b"<!DOCTYPE html><html>").unwrap(),
            Format::Html
        );
        assert_eq!(detect_format(None, b"<html><body>").unwrap(), Format::Html);
    }

    #[test]
    fn defaults_to_plaintext_when_unknown() {
        assert_eq!(
            detect_format(Some("file.xyz"), b"just some text").unwrap(),
            Format::PlainText
        );
        assert_eq!(
            detect_format(None, b"just some text").unwrap(),
            Format::PlainText
        );
    }

    #[test]
    fn case_insensitive_extension() {
        assert_eq!(
            detect_format(Some("README.MD"), b"# Title").unwrap(),
            Format::Markdown
        );
        assert_eq!(
            detect_format(Some("PAGE.HTML"), b"<html>").unwrap(),
            Format::Html
        );
    }
}
