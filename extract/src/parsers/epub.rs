//! EPUB parser: spine chapters → Markdown via `rbook` + `anytomd`.
//!
//! An EPUB is a ZIP of XHTML chapters plus an OPF manifest whose metadata is
//! literally Dublin Core — a 1:1 fit for [`DocumentMetadata`]. We iterate the
//! spine in canonical reading order, convert each chapter's XHTML to Markdown
//! (no readability pruning — see [`crate::html::xhtml_to_markdown`]), and join
//! the chapters with a blank-line separator.

use localdb_core::parser::{DocumentMetadata, ParsedDocument, Parser, Probe};
use localdb_core::Error;
use rbook::Epub;

/// Handles EPUB files identified by the `.epub` extension.
///
/// Declines all other inputs. Open/parse failure and effectively-empty books
/// (DRM'd / image-only) return `Err`, short-circuiting the chain so plaintext
/// does not silently grab them.
pub struct EpubParser;

/// Separator placed between chapters when joining the spine into one string.
const CHAPTER_SEP: &str = "\n\n";

impl Parser for EpubParser {
    fn id(&self) -> &'static str {
        "epub"
    }

    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        let by_ext = probe
            .extension()
            .map(|e| e.eq_ignore_ascii_case("epub"))
            .unwrap_or(false);
        if !by_ext {
            return Ok(None);
        }

        // rbook needs `Read + Seek` owning the bytes ('static under the
        // threadsafe feature), so hand it an owned cursor.
        let cursor = std::io::Cursor::new(probe.bytes().to_vec());
        let epub = Epub::read(cursor).map_err(|e| Error::ExtractionFailed {
            format: "epub".to_string(),
            reason: e.to_string(),
        })?;

        // Iterate the spine in canonical reading order, converting each chapter.
        // Chapter-level errors (ill-formed XHTML) are non-fatal: skip the bad
        // chapter and continue so the rest of the book is still indexed.
        let mut chapters: Vec<String> = Vec::new();
        let mut reader = epub.reader();
        while let Some(item) = reader.read_next() {
            let content = match item {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        path = probe.path_hint.unwrap_or("?"),
                        "skipping malformed EPUB chapter: {e}"
                    );
                    continue;
                }
            };
            let md = match crate::html::xhtml_to_markdown(content.content().as_bytes()) {
                Ok(md) => md,
                Err(e) => {
                    tracing::warn!(
                        path = probe.path_hint.unwrap_or("?"),
                        "skipping EPUB chapter (XHTML→Markdown failed): {e}"
                    );
                    continue;
                }
            };
            let md = md.trim();
            if !md.is_empty() {
                chapters.push(md.to_string());
            }
        }

        let markdown = chapters.join(CHAPTER_SEP);

        // Guard against DRM'd / image-only books that yield no readable text.
        if markdown.trim().is_empty() {
            return Err(Error::ExtractionFailed {
                format: "epub".to_string(),
                reason: "no readable text extracted (empty, image-only, or DRM-protected book)"
                    .to_string(),
            });
        }

        let metadata = map_metadata(&epub, probe);
        let title = metadata.title.clone();

        Ok(Some(ParsedDocument {
            markdown,
            title,
            metadata,
        }))
    }
}

/// Map OPF Dublin Core metadata onto [`DocumentMetadata`].
fn map_metadata(epub: &Epub, probe: &Probe) -> DocumentMetadata {
    let meta = epub.metadata();

    let creator: Vec<String> = meta.creators().map(|c| c.value().to_string()).collect();
    let contributor: Vec<String> = meta.contributors().map(|c| c.value().to_string()).collect();
    let subject: Vec<String> = meta.tags().map(|t| t.value().to_string()).collect();

    DocumentMetadata {
        title: meta.title().map(|t| t.value().to_string()),
        creator,
        subject,
        description: meta.description().map(|d| d.value().to_string()),
        publisher: meta.publishers().next().map(|p| p.value().to_string()),
        contributor,
        date: meta.published().map(|d| d.date().to_string()),
        format: probe.sniffed_mime.map(|s| s.to_string()),
        identifier: meta.identifier().map(|i| i.value().to_string()),
        language: meta.language().map(|l| l.value().to_string()),
        rights: meta.rights().next().map(|r| r.value().to_string()),
        ..DocumentMetadata::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::parser::Probe;

    fn sample_epub() -> Vec<u8> {
        std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.epub"
        ))
        .expect("sample.epub fixture must exist")
    }

    #[test]
    fn declines_pdf_extension() {
        let probe = Probe::new(b"%PDF-1.4\n", Some("doc.pdf"), None);
        assert!(EpubParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_no_extension() {
        let probe = Probe::new(b"PK\x03\x04 some zip", None, None);
        assert!(EpubParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn declines_html_extension() {
        let probe = Probe::new(b"<html>...</html>", Some("page.html"), None);
        assert!(EpubParser.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn garbage_epub_returns_extraction_failed() {
        let probe = Probe::new(b"this is not a zip file at all!", Some("book.epub"), None);
        match EpubParser.parse(&probe) {
            Err(Error::ExtractionFailed { format, .. }) => assert_eq!(format, "epub"),
            other => panic!("expected ExtractionFailed, got: {other:?}"),
        }
    }

    #[test]
    fn empty_epub_returns_extraction_failed() {
        // A valid (empty) ZIP container is not a valid EPUB → open fails as
        // ExtractionFailed. This also exercises the error path for malformed books.
        let empty_zip =
            b"PK\x05\x06\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        let probe = Probe::new(empty_zip, Some("empty.epub"), None);
        match EpubParser.parse(&probe) {
            Err(Error::ExtractionFailed { format, .. }) => assert_eq!(format, "epub"),
            other => panic!("expected ExtractionFailed, got: {other:?}"),
        }
    }

    /// Build a minimal in-memory EPUB (valid ZIP + OPF) whose spine contains
    /// a malformed XHTML chapter followed by a well-formed one.  Used to
    /// verify the skip-and-continue behaviour introduced to fix the
    /// "ill-formed document" regression.
    fn make_epub_with_malformed_chapter() -> Vec<u8> {
        use std::io::Write as _;
        use zip::write::SimpleFileOptions;
        use zip::{CompressionMethod, ZipWriter};

        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buf);

        // EPUB spec: mimetype first, stored (uncompressed), no extra fields.
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        zip.start_file("mimetype", stored).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();

        zip.start_file("META-INF/container.xml", deflated).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf"
              media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
        )
        .unwrap();

        // Spine: bad chapter first, good chapter second.  This exercises
        // "skip-and-continue": the error on the first item must not abort the loop.
        zip.start_file("OEBPS/content.opf", deflated).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Regression Book</dc:title>
    <dc:identifier id="uid">reg-test-id</dc:identifier>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="bad"  href="bad.xhtml"  media-type="application/xhtml+xml"/>
    <item id="good" href="good.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="bad"/>
    <itemref idref="good"/>
  </spine>
</package>"#,
        )
        .unwrap();

        // Malformed XHTML: unescaped `&` is invalid XML and must trigger a
        // chapter-level parse error from rbook's strict XML reader.
        zip.start_file("OEBPS/bad.xhtml", deflated).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Bad</title></head>
<body><p>AT&T invalid XML entity</p></body>
</html>"#,
        )
        .unwrap();

        // Well-formed XHTML with text we can assert on.
        zip.start_file("OEBPS/good.xhtml", deflated).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Good</title></head>
<body><p>good chapter content that must survive</p></body>
</html>"#,
        )
        .unwrap();

        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn malformed_chapter_is_skipped_good_content_preserved() {
        // Regression: a single ill-formed XHTML chapter (e.g. unescaped `&`)
        // must not abort the whole book — the good chapter must still be indexed.
        let epub_bytes = make_epub_with_malformed_chapter();
        let probe = Probe::new(&epub_bytes, Some("partial.epub"), None);
        let doc = EpubParser.parse(&probe).unwrap().unwrap();
        assert!(
            doc.markdown
                .contains("good chapter content that must survive"),
            "good chapter content must be present despite the malformed chapter; \
             got: {:?}",
            &doc.markdown[..doc.markdown.len().min(200)]
        );
    }

    #[test]
    fn sample_epub_extracts_both_chapters_in_order() {
        let bytes = sample_epub();
        let probe = Probe::new(&bytes, Some("sample.epub"), Some("application/epub+zip"));
        let doc = EpubParser.parse(&probe).unwrap().unwrap();

        let one = doc
            .markdown
            .find("bright cold day")
            .expect("chapter one text must be present");
        let two = doc
            .markdown
            .find("second chapter continues")
            .expect("chapter two text must be present");
        assert!(one < two, "chapters must appear in spine reading order");
    }

    #[test]
    fn sample_epub_maps_dublin_core_metadata() {
        let bytes = sample_epub();
        let probe = Probe::new(&bytes, Some("sample.epub"), Some("application/epub+zip"));
        let doc = EpubParser.parse(&probe).unwrap().unwrap();
        let m = &doc.metadata;

        assert_eq!(m.title.as_deref(), Some("The Great Adventure"));
        assert_eq!(doc.title.as_deref(), Some("The Great Adventure"));
        assert_eq!(m.creator, vec!["Jane Author".to_string()]);
        assert_eq!(m.language.as_deref(), Some("en"));
        assert_eq!(m.identifier.as_deref(), Some("urn:isbn:9781234567890"));
        assert_eq!(m.publisher.as_deref(), Some("Test House Press"));
        assert_eq!(m.date.as_deref(), Some("2021-05-01"));
        assert_eq!(m.format.as_deref(), Some("application/epub+zip"));
        assert!(m.subject.contains(&"Fiction".to_string()));
        assert!(m.subject.contains(&"Testing".to_string()));
        assert_eq!(m.rights.as_deref(), Some("Public Domain"));
        assert_eq!(
            m.description.as_deref(),
            Some("A short tale used as a test fixture.")
        );
    }
}
