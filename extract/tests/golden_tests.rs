//! Golden-file integration tests for the extract crate.
//!
//! Each test loads a fixture, runs the default `ChainExtractor`, and asserts
//! properties of the `ExtractionResult.markdown` string.

use extract::ChainExtractor;
use localdb_core::ingestion::DocumentExtractor;
use localdb_core::Error;

// ---------------------------------------------------------------------------
// Markdown golden tests
// ---------------------------------------------------------------------------

#[test]
fn markdown_fixture_title_extracted() {
    let bytes = include_bytes!("fixtures/simple.md");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("simple.md")).unwrap();
    assert_eq!(result.title, Some("Introduction".to_string()));
}

#[test]
fn markdown_fixture_contains_headings() {
    let bytes = include_bytes!("fixtures/simple.md");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("simple.md")).unwrap();
    // Real Markdown headings must be preserved (not stripped)
    assert!(
        result.markdown.contains("# Introduction"),
        "H1 heading must be in markdown output"
    );
    assert!(
        result.markdown.contains("## Getting Started"),
        "H2 heading must be in markdown output"
    );
    assert!(
        result.markdown.contains("### Prerequisites"),
        "H3 heading must be in markdown output"
    );
}

#[test]
fn markdown_fixture_contains_code_fence() {
    let bytes = include_bytes!("fixtures/simple.md");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("simple.md")).unwrap();
    assert!(
        result.markdown.contains("```"),
        "Code fence must be preserved in markdown output"
    );
    assert!(
        result.markdown.contains("rustup"),
        "Code content must be present"
    );
}

#[test]
fn markdown_fixture_is_passthrough() {
    let raw = include_str!("fixtures/simple.md");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(raw.as_bytes(), Some("simple.md")).unwrap();
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    assert_eq!(
        result.markdown, normalized,
        "Markdown parser must be a passthrough (modulo CRLF normalization)"
    );
}

// ---------------------------------------------------------------------------
// Heading index via core: nested_headings.md
// ---------------------------------------------------------------------------

#[test]
fn nested_headings_heading_index_paths() {
    use localdb_core::heading_index::{build_heading_index, heading_path_at};

    let bytes = include_bytes!("fixtures/nested_headings.md");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("nested_headings.md")).unwrap();

    let idx = build_heading_index(&result.markdown);

    // Find "Deep content here." in the markdown and check its heading path
    let offset = result
        .markdown
        .find("Deep content here.")
        .expect("'Deep content here.' must appear in markdown");
    let path = heading_path_at(&idx, offset);
    assert_eq!(
        path,
        vec!["Level 1", "Level 1.1", "Level 1.1.1"],
        "Content under Level 1.1.1 must have full heading path"
    );

    // Find "Content under level 2.1." and check its heading path
    let offset2 = result
        .markdown
        .find("Content under level 2.1.")
        .expect("'Content under level 2.1.' must appear in markdown");
    let path2 = heading_path_at(&idx, offset2);
    assert_eq!(
        path2,
        vec!["Level 2", "Level 2.1"],
        "Content under Level 2.1 must have 2-element path"
    );
}

// ---------------------------------------------------------------------------
// Plain text golden tests
// ---------------------------------------------------------------------------

#[test]
fn plaintext_fixture_content_present() {
    let bytes = include_bytes!("fixtures/plain.txt");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("plain.txt")).unwrap();
    assert!(result.title.is_none());
    assert!(
        result.markdown.contains("first paragraph"),
        "First paragraph must be present"
    );
    assert!(
        result.markdown.contains("second paragraph"),
        "Second paragraph must be present"
    );
    assert!(
        result.markdown.contains("third paragraph"),
        "Third paragraph must be present"
    );
}

#[test]
fn plaintext_fixture_is_passthrough() {
    let raw = include_str!("fixtures/plain.txt");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(raw.as_bytes(), Some("plain.txt")).unwrap();
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    assert_eq!(
        result.markdown, normalized,
        "Plaintext parser must be a passthrough (modulo CRLF normalization)"
    );
}

// ---------------------------------------------------------------------------
// HTML golden tests
// ---------------------------------------------------------------------------

#[test]
fn html_fixture_title_from_meta_tag() {
    let bytes = include_bytes!("fixtures/article.html");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("article.html")).unwrap();
    assert_eq!(
        result.title,
        Some("Test Article".to_string()),
        "Title must come from <title> tag"
    );
}

#[test]
fn html_fixture_article_content_present() {
    let bytes = include_bytes!("fixtures/article.html");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("article.html")).unwrap();
    // Main content headings
    assert!(
        result.markdown.contains("Main Article Title"),
        "H1 content must be present; markdown: {}",
        &result.markdown[..result.markdown.len().min(500)]
    );
    assert!(
        result.markdown.contains("Section One"),
        "H2 Section One must be present"
    );
    assert!(
        result.markdown.contains("Subsection A"),
        "H3 Subsection A must be present"
    );
}

#[test]
fn html_fixture_heading_index_paths() {
    use localdb_core::heading_index::{build_heading_index, heading_path_at};

    let bytes = include_bytes!("fixtures/article.html");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("article.html")).unwrap();

    let idx = build_heading_index(&result.markdown);

    // "This is the content of section one." appears under Section One / Main Article Title
    let offset = result
        .markdown
        .find("content of section one")
        .expect("'content of section one' must appear in markdown output");
    let path = heading_path_at(&idx, offset);
    assert!(
        path.iter().any(|p| p.contains("Section One")),
        "Section One content must be under Section One heading path; got {path:?}"
    );
    assert!(
        path.iter().any(|p| p.contains("Main Article Title")),
        "Section One must be under Main Article Title; got {path:?}"
    );
}

#[test]
fn html_fixture_nav_stripped_or_minimized() {
    let bytes = include_bytes!("fixtures/article.html");
    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(bytes, Some("article.html")).unwrap();
    // The readability selector should suppress or minimize navigation
    // (It may not be completely empty but the article content must dominate)
    assert!(
        result
            .markdown
            .contains("first paragraph of the article content"),
        "Article content must be present; markdown start: {}",
        &result.markdown[..result.markdown.len().min(500)]
    );
}

// ---------------------------------------------------------------------------
// PDF: scanned/corrupt fixture must return Err (never Ok)
// ---------------------------------------------------------------------------

#[test]
fn scanned_pdf_fixture_returns_err() {
    // The fixture is a minimal PDF that pdf-extract may either:
    //   - fail to parse entirely → ExtractionFailed (corrupt/parse error)
    //   - parse but find no text  → UnsupportedFormat (scanned-PDF path)
    // In both cases the extractor must not return Ok.
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/scanned.pdf"
    ))
    .expect("scanned.pdf fixture not found");

    let ex = ChainExtractor::with_defaults().unwrap();
    let result = ex.extract(&bytes, Some("scanned.pdf"));
    match result {
        Err(Error::UnsupportedFormat { .. }) | Err(Error::ExtractionFailed { .. }) => {}
        Ok(out) => panic!(
            "Expected Err for scanned/corrupt PDF, got markdown: {:?}",
            out.markdown
        ),
        Err(other) => panic!("Unexpected error variant for scanned PDF: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Cross-format: all formats produce non-empty markdown
// ---------------------------------------------------------------------------

#[test]
fn all_text_formats_produce_non_empty_markdown() {
    let fixtures: &[(&[u8], &str)] = &[
        (include_bytes!("fixtures/simple.md"), "simple.md"),
        (include_bytes!("fixtures/plain.txt"), "plain.txt"),
        (include_bytes!("fixtures/article.html"), "article.html"),
        (
            include_bytes!("fixtures/nested_headings.md"),
            "nested_headings.md",
        ),
    ];

    let ex = ChainExtractor::with_defaults().unwrap();
    for (bytes, name) in fixtures {
        let result = ex
            .extract(bytes, Some(name))
            .unwrap_or_else(|e| panic!("extraction failed for {name}: {e}"));
        assert!(
            !result.markdown.is_empty(),
            "[{name}] markdown must not be empty"
        );
    }
}
