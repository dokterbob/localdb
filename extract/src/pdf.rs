//! PDF extraction via `pdf_oxide`: per-page Markdown plus page-start offsets.
//!
//! Each page is converted to Markdown independently and concatenated;
//! `PdfExtract::page_starts` records the byte offset where each page's content
//! begins, which downstream block building resolves into per-block page
//! numbers (#103).
//!
//! Scanned PDFs (no text layer) yield [`Error::UnsupportedFormat`], not
//! garbage text. No pdf_oxide type leaks out of this module — the rest of the
//! crate sees only [`PdfExtract`], keeping a parser swap a one-file change.

use localdb_core::Error;
use pdf_oxide::converters::ConversionOptions;
use pdf_oxide::PdfDocument;

/// Minimum ratio of printable characters required to consider a PDF text-bearing.
///
/// Below this threshold the PDF is treated as a scanned image document.
const MIN_PRINTABLE_RATIO: f64 = 0.1;

/// Minimum absolute character count to consider a PDF text-bearing.
const MIN_TEXT_CHARS: usize = 20;

/// Result of PDF extraction: Markdown, document title, and page-start offsets.
#[derive(Debug, Clone)]
pub struct PdfExtract {
    /// The whole document as Markdown, pages concatenated in order.
    pub markdown: String,
    /// Document title from the Info dictionary or XMP metadata.
    pub title: Option<String>,
    /// `(byte_offset, page_number)` for every page that contributed content,
    /// ascending in both fields. `byte_offset` indexes into `markdown`;
    /// `page_number` is 1-based. Pages that yielded no text are absent.
    pub page_starts: Vec<(usize, u32)>,
}

/// Extract a PDF into Markdown with per-page offsets and a title.
///
/// Returns [`Error::ExtractionFailed`] for corrupt/malformed PDFs where no
/// page could be extracted, and [`Error::UnsupportedFormat`] for scanned
/// (no text layer) or password-protected PDFs.
///
/// A page that individually fails to convert is skipped with a warning —
/// one broken page must not lose a whole book — but if *every* page fails
/// the document as a whole is an extraction failure.
pub fn extract_pdf(bytes: &[u8]) -> Result<PdfExtract, Error> {
    let doc = PdfDocument::from_bytes(bytes.to_vec()).map_err(|e| Error::ExtractionFailed {
        format: "pdf".into(),
        reason: e.to_string(),
    })?;

    // Encrypted and not decryptable with the empty owner/user password:
    // `to_markdown` would silently return empty pages, which would then be
    // misreported as "scanned". Fail with an honest reason instead.
    if !doc.is_authenticated() {
        return Err(Error::UnsupportedFormat {
            format: "pdf (encrypted — password required)".to_string(),
        });
    }

    let page_count = doc.page_count().map_err(|e| Error::ExtractionFailed {
        format: "pdf".into(),
        reason: e.to_string(),
    })?;

    let options = ConversionOptions::default();
    let mut markdown = String::new();
    let mut page_starts: Vec<(usize, u32)> = Vec::new();
    let mut ok_pages = 0usize;
    let mut last_err: Option<String> = None;

    for page in 0..page_count {
        match doc.to_markdown(page, &options) {
            Ok(md) => {
                ok_pages += 1;
                let trimmed = md.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !markdown.is_empty() {
                    markdown.push_str("\n\n");
                }
                page_starts.push((markdown.len(), (page + 1) as u32));
                markdown.push_str(trimmed);
            }
            Err(e) => {
                tracing::warn!(page = page + 1, error = %e, "skipping unextractable PDF page");
                last_err = Some(e.to_string());
            }
        }
    }

    if ok_pages == 0 {
        return Err(Error::ExtractionFailed {
            format: "pdf".into(),
            reason: last_err.unwrap_or_else(|| "PDF has no extractable pages".to_string()),
        });
    }

    if is_scanned_pdf(&markdown) {
        return Err(Error::UnsupportedFormat {
            format: "pdf (scanned — no text layer detected)".to_string(),
        });
    }

    if !markdown.ends_with('\n') {
        markdown.push('\n');
    }

    let title = document_title(&doc);
    Ok(PdfExtract {
        markdown,
        title,
        page_starts,
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

/// Document title: Info dictionary `/Title` first (the canonical viewer
/// title), XMP `dc:title` as fallback.
fn document_title(doc: &PdfDocument) -> Option<String> {
    info_dict_title(doc).or_else(|| xmp_title(doc))
}

/// `/Title` from the trailer's `/Info` dictionary, decoded and trimmed.
fn info_dict_title(doc: &PdfDocument) -> Option<String> {
    let info_raw = doc.trailer().as_dict()?.get("Info")?.clone();
    let info = doc.resolve_references(&info_raw, 2).ok()?;
    let title_raw = info.as_dict()?.get("Title")?.clone();
    let title = doc.resolve_references(&title_raw, 2).ok()?;
    let decoded = decode_pdf_text_string(title.as_string()?);
    let trimmed = decoded.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `dc:title` from XMP metadata, trimmed.
fn xmp_title(doc: &PdfDocument) -> Option<String> {
    let xmp = pdf_oxide::extractors::xmp::XmpExtractor::extract(doc)
        .ok()
        .flatten()?;
    let title = xmp.dc_title?;
    let trimmed = title.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Decode a PDF text string: UTF-16BE with BOM, UTF-8, or byte-per-char
/// (PDFDocEncoding approximated as Latin-1 — exact for ASCII, close enough
/// for a title).
fn decode_pdf_text_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else if let Ok(s) = std::str::from_utf8(bytes) {
        s.to_string()
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::Error;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {name} must exist: {e}"))
    }

    /// A malformed fixture must never panic; it may recover (Ok) or fail
    /// with one of the two documented error variants.
    fn assert_no_panic_and_sane(name: &str) -> Result<PdfExtract, Error> {
        let result = extract_pdf(&fixture(name));
        match &result {
            Ok(_) | Err(Error::ExtractionFailed { .. }) | Err(Error::UnsupportedFormat { .. }) => {}
            Err(other) => panic!("{name}: unexpected error variant: {other:?}"),
        }
        result
    }

    // ------------------------------------------------------------------
    // Malformed-PDF fixtures (the #87 class): Err or recovery, never panic.
    // ------------------------------------------------------------------

    #[test]
    fn zero_operand_operators_do_not_panic() {
        // Content stream starts with operand-less Tj/Td/Tf/TJ — the exact
        // class that made pdf-extract panic with "index out of bounds".
        if let Ok(ex) = assert_no_panic_and_sane("malformed/zero_operand_ops.pdf") {
            // If recovery succeeds, the valid trailing text should be there.
            assert!(
                ex.markdown.contains("Recovered text"),
                "recovered output should contain the valid text run: {:?}",
                ex.markdown
            );
        }
    }

    #[test]
    fn truncated_stream_does_not_panic() {
        let _ = assert_no_panic_and_sane("malformed/truncated_stream.pdf");
    }

    #[test]
    fn broken_xref_does_not_panic() {
        let _ = assert_no_panic_and_sane("malformed/broken_xref.pdf");
    }

    #[test]
    fn empty_page_pdf_returns_err() {
        // Structurally valid, but a single page with no /Contents: nothing
        // to index, so this must be an error, not Ok("").
        let result = assert_no_panic_and_sane("malformed/empty_page.pdf");
        assert!(result.is_err(), "empty-page PDF must not yield Ok");
    }

    #[test]
    fn cid_font_without_tounicode_yields_no_mojibake() {
        // Type0/Identity-H font with no /ToUnicode and no embedded font
        // program: glyphs cannot be mapped. Acceptable outcomes are an
        // error or output without U+FFFD replacement chars — never mojibake.
        if let Ok(ex) = assert_no_panic_and_sane("malformed/cid_no_tounicode.pdf") {
            assert!(
                !ex.markdown.contains('\u{FFFD}'),
                "unmappable glyphs must not surface as replacement chars: {:?}",
                ex.markdown
            );
        }
    }

    #[test]
    fn garbage_bytes_return_extraction_failed() {
        let result = extract_pdf(b"%PDF-1.4\nnot a real pdf");
        match result {
            Err(Error::ExtractionFailed { .. }) | Err(Error::UnsupportedFormat { .. }) => {}
            Ok(ex) => panic!("garbage input should not extract: {:?}", ex.markdown),
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Happy path: multi-page extraction with page offsets and title.
    // ------------------------------------------------------------------

    #[test]
    fn multipage_page_starts_are_correct() {
        let ex = extract_pdf(&fixture("multipage.pdf")).expect("multipage fixture must extract");

        let pages: Vec<u32> = ex.page_starts.iter().map(|&(_, p)| p).collect();
        assert_eq!(pages, vec![1, 2, 3], "all three pages must contribute");

        // Offsets strictly ascending and in bounds.
        let offsets: Vec<usize> = ex.page_starts.iter().map(|&(o, _)| o).collect();
        assert_eq!(offsets[0], 0, "page 1 starts at offset 0");
        assert!(offsets.windows(2).all(|w| w[0] < w[1]));
        assert!(*offsets.last().unwrap() < ex.markdown.len());

        // Distinctive per-page content lands within that page's span.
        let find = |needle: &str| {
            ex.markdown
                .find(needle)
                .unwrap_or_else(|| panic!("markdown must contain {needle:?}: {:?}", ex.markdown))
        };
        assert!(
            find("quick brown fox") < offsets[1],
            "page-1 text before page 2 start"
        );
        let sphinx = find("Sphinx of black quartz");
        assert!(
            (offsets[1]..offsets[2]).contains(&sphinx),
            "page-2 text within page 2 span"
        );
        assert!(
            find("Pack my box") >= offsets[2],
            "page-3 text after page 3 start"
        );
    }

    #[test]
    fn multipage_title_from_info_dict() {
        let ex = extract_pdf(&fixture("multipage.pdf")).expect("multipage fixture must extract");
        assert_eq!(ex.title.as_deref(), Some("Multipage Fixture Title"));
    }

    #[test]
    fn flat_body_text_gets_no_hallucinated_headings() {
        // Uniform 11pt body text: heading detection must not invent
        // structure (protects #158's coarse-Text chunk packing).
        let ex = extract_pdf(&fixture("flat_body.pdf")).expect("flat_body fixture must extract");
        for line in ex.markdown.lines() {
            assert!(
                !line.trim_start().starts_with('#'),
                "no line should become a heading, got: {line:?}"
            );
        }
    }

    #[test]
    fn extraction_ends_with_newline() {
        let ex = extract_pdf(&fixture("multipage.pdf")).expect("multipage fixture must extract");
        assert!(ex.markdown.ends_with('\n'));
    }

    // ------------------------------------------------------------------
    // Scanned-PDF heuristic (unchanged semantics).
    // ------------------------------------------------------------------

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
    fn decode_pdf_text_string_handles_utf16be_bom() {
        let bytes = [0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69]; // "Hi"
        assert_eq!(decode_pdf_text_string(&bytes), "Hi");
    }

    #[test]
    fn decode_pdf_text_string_handles_plain_ascii() {
        assert_eq!(decode_pdf_text_string(b"Plain Title"), "Plain Title");
    }

    #[test]
    fn decode_pdf_text_string_handles_latin1_bytes() {
        // 0xE9 = é in Latin-1; invalid as standalone UTF-8.
        assert_eq!(decode_pdf_text_string(&[0x63, 0x61, 0x66, 0xE9]), "café");
    }
}
