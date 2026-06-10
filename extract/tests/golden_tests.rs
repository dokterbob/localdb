//! Golden-file integration tests for the extract crate.
//!
//! Each test loads a fixture file and asserts the expected extraction output.

use extract::{detect_format, extract, Format};
use localdb_core::{BlockKind, Error};

// ---------------------------------------------------------------------------
// Format detection tests
// ---------------------------------------------------------------------------

#[test]
fn detect_markdown_fixture() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/simple.md"
    ))
    .unwrap();
    assert_eq!(
        detect_format(Some("simple.md"), &bytes).unwrap(),
        Format::Markdown
    );
}

#[test]
fn detect_plaintext_fixture() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/plain.txt"
    ))
    .unwrap();
    assert_eq!(
        detect_format(Some("plain.txt"), &bytes).unwrap(),
        Format::PlainText
    );
}

#[test]
fn detect_html_fixture() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/article.html"
    ))
    .unwrap();
    assert_eq!(
        detect_format(Some("article.html"), &bytes).unwrap(),
        Format::Html
    );
}

// ---------------------------------------------------------------------------
// Markdown golden tests
// ---------------------------------------------------------------------------

#[test]
fn markdown_fixture_all_spans_valid() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/simple.md"
    ))
    .unwrap();
    let out = extract(&bytes, Some("simple.md")).unwrap();

    for block in &out.blocks {
        assert!(
            block.span.end <= out.text.len(),
            "Span end {} exceeds text length {} for block {:?}",
            block.span.end,
            out.text.len(),
            block
        );
        assert!(
            block.span.start <= block.span.end,
            "Span start {} > end {} for block {:?}",
            block.span.start,
            block.span.end,
            block
        );
        // Span content must contain the block text
        let span_text = &out.text[block.span.start..block.span.end];
        assert!(
            span_text.contains(block.text.trim()),
            "Span [{}, {}] = {:?} should contain block text {:?}",
            block.span.start,
            block.span.end,
            span_text,
            block.text
        );
    }
}

#[test]
fn markdown_fixture_has_expected_structure() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/simple.md"
    ))
    .unwrap();
    let out = extract(&bytes, Some("simple.md")).unwrap();

    // Title should be "Introduction"
    assert_eq!(out.title, Some("Introduction".to_string()));

    // Should have multiple headings
    let headings: Vec<_> = out
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Heading)
        .collect();
    assert!(
        headings.len() >= 3,
        "Expected at least 3 headings in simple.md fixture"
    );

    // Should have code blocks
    let code_blocks: Vec<_> = out
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Code)
        .collect();
    assert!(
        !code_blocks.is_empty(),
        "Expected at least one code block in simple.md fixture"
    );
}

#[test]
fn markdown_nested_headings_correct_paths() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/nested_headings.md"
    ))
    .unwrap();
    let out = extract(&bytes, Some("nested_headings.md")).unwrap();

    // Check that Level 1.1.1 heading has full path
    let h3_block = out
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Heading && b.text == "Level 1.1.1")
        .expect("Expected 'Level 1.1.1' heading");
    assert_eq!(
        h3_block.heading_path,
        vec!["Level 1", "Level 1.1", "Level 1.1.1"]
    );

    // Check that the content under it has the correct path
    let deep_content = out
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Paragraph && b.text.contains("Deep content"))
        .expect("Expected 'Deep content' paragraph");
    assert_eq!(
        deep_content.heading_path,
        vec!["Level 1", "Level 1.1", "Level 1.1.1"]
    );

    // Level 2.1 should have path [Level 2, Level 2.1]
    let l2_block = out
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Heading && b.text == "Level 2.1")
        .expect("Expected 'Level 2.1' heading");
    assert_eq!(l2_block.heading_path, vec!["Level 2", "Level 2.1"]);
}

// ---------------------------------------------------------------------------
// Plain text golden tests
// ---------------------------------------------------------------------------

#[test]
fn plaintext_fixture_produces_three_paragraphs() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/plain.txt"
    ))
    .unwrap();
    let out = extract(&bytes, Some("plain.txt")).unwrap();

    assert_eq!(
        out.blocks.len(),
        3,
        "Expected 3 paragraphs, got {}: {:?}",
        out.blocks.len(),
        out.blocks.iter().map(|b| &b.text).collect::<Vec<_>>()
    );
    assert!(out.blocks.iter().all(|b| b.kind == BlockKind::Paragraph));
}

#[test]
fn plaintext_fixture_all_spans_valid() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/plain.txt"
    ))
    .unwrap();
    let out = extract(&bytes, Some("plain.txt")).unwrap();

    for block in &out.blocks {
        let span_text = &out.text[block.span.start..block.span.end];
        assert!(
            span_text.contains(block.text.trim()),
            "Span should contain block text"
        );
    }
}

// ---------------------------------------------------------------------------
// HTML golden tests
// ---------------------------------------------------------------------------

#[test]
fn html_fixture_extracts_article_content() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/article.html"
    ))
    .unwrap();
    let out = extract(&bytes, Some("article.html")).unwrap();

    // Should have a title
    assert!(out.title.is_some(), "Expected a title from HTML fixture");

    // Should have headings
    let headings: Vec<_> = out
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Heading)
        .collect();
    assert!(!headings.is_empty(), "Expected at least one heading");

    // Should have paragraphs
    let paras: Vec<_> = out
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Paragraph)
        .collect();
    assert!(!paras.is_empty(), "Expected at least one paragraph");
}

#[test]
fn html_fixture_heading_paths_correct() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/article.html"
    ))
    .unwrap();
    let out = extract(&bytes, Some("article.html")).unwrap();

    // "Section One" is H2 under H1 "Main Article Title"
    let section_one = out
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Heading && b.text.contains("Section One"))
        .expect("Expected 'Section One' heading in HTML fixture");
    assert!(
        section_one
            .heading_path
            .contains(&"Main Article Title".to_string()),
        "Section One should be under Main Article Title, got {:?}",
        section_one.heading_path
    );

    // "Subsection A" is H3
    let subsection = out
        .blocks
        .iter()
        .find(|b| b.kind == BlockKind::Heading && b.text.contains("Subsection A"))
        .expect("Expected 'Subsection A' heading");
    assert_eq!(
        subsection.heading_path,
        vec!["Main Article Title", "Section One", "Subsection A"]
    );
}

#[test]
fn html_fixture_all_spans_valid() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/article.html"
    ))
    .unwrap();
    let out = extract(&bytes, Some("article.html")).unwrap();

    for block in &out.blocks {
        assert!(
            block.span.end <= out.text.len(),
            "Span end {} exceeds text length {}",
            block.span.end,
            out.text.len()
        );
        let span_text = &out.text[block.span.start..block.span.end];
        assert!(
            span_text.contains(block.text.trim()),
            "Span should contain block text {:?}, got {:?}",
            block.text,
            span_text
        );
    }
}

// ---------------------------------------------------------------------------
// PDF tests: unsupported_format for scanned/empty PDFs
// ---------------------------------------------------------------------------

/// Golden test: the in-repo scanned PDF fixture must yield `unsupported_format`.
///
/// Acceptance criterion (PLAN.md T04): "a scanned-PDF fixture yields
/// `unsupported_format`, not garbage text."  The fixture is a valid PDF with an
/// empty content stream — no text operators at all.
#[test]
fn scanned_pdf_fixture_returns_unsupported_format() {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/scanned.pdf"
    ))
    .expect("scanned.pdf fixture not found");

    let result = extract(&bytes, Some("scanned.pdf"));
    match result {
        Err(Error::UnsupportedFormat { .. }) => {
            // Expected: scanned PDF correctly returns UnsupportedFormat
        }
        Ok(out) => {
            panic!(
                "Expected UnsupportedFormat for scanned PDF fixture, \
                 but extraction succeeded with {} blocks and text {:?}",
                out.blocks.len(),
                out.text
            );
        }
        Err(other) => {
            panic!(
                "Expected UnsupportedFormat for scanned PDF fixture, got {:?}",
                other
            );
        }
    }
}

#[test]
fn non_pdf_bytes_with_pdf_extension() {
    // File named .pdf but content is not a PDF
    let not_really_pdf = b"This is just a text file";
    let result = extract(not_really_pdf, Some("fake.pdf"));
    // Should either return UnsupportedFormat or an extraction error
    match result {
        Err(Error::UnsupportedFormat { .. }) | Err(Error::InvalidRequest { .. }) => {
            // Expected
        }
        Ok(_) => {
            // pdf-extract might handle this differently — just ensure we don't panic
        }
        Err(other) => {
            panic!("Unexpected error: {:?}", other);
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-format: spans always index into normalized text
// ---------------------------------------------------------------------------

#[test]
fn all_formats_produce_valid_spans() {
    let fixtures: &[(&[u8], &str)] = &[
        (include_bytes!("fixtures/simple.md"), "simple.md"),
        (include_bytes!("fixtures/plain.txt"), "plain.txt"),
        (include_bytes!("fixtures/article.html"), "article.html"),
    ];

    for (bytes, name) in fixtures {
        let out =
            extract(bytes, Some(name)).unwrap_or_else(|_| panic!("extraction failed for {name}"));
        for block in &out.blocks {
            assert!(
                block.span.start <= block.span.end,
                "[{name}] Block span start > end"
            );
            assert!(
                block.span.end <= out.text.len(),
                "[{name}] Block span end {} > text length {}",
                block.span.end,
                out.text.len()
            );
            let span_text = &out.text[block.span.start..block.span.end];
            assert!(
                span_text.contains(block.text.trim()),
                "[{name}] Span {:?} should contain block text {:?}",
                span_text,
                block.text
            );
        }
    }
}
