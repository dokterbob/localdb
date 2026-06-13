//! HTML extraction with readability-style main-content selection.
//!
//! Walks the DOM to produce normalized text + Block structures.
//! Heading paths are tracked as in the Markdown extractor.
//!
//! Readability heuristic: prefer `<article>`, `<main>`, or `[role="main"]`
//! if present; otherwise walk the full `<body>`.

use crate::{make_block, ExtractionOutput};
use localdb_core::{BlockKind, Error, Span};
use scraper::{Html, Selector};

/// Extract an HTML document.
///
/// The main content element is selected using a readability-style heuristic.
/// Headings, paragraphs, code blocks, lists, and blockquotes are extracted.
pub fn extract_html(input: &str) -> Result<ExtractionOutput, Error> {
    let document = Html::parse_document(input);

    // Extract the title from <title> tag
    let page_title = extract_page_title(&document);

    // Find main content container using readability heuristic
    let main_html = extract_main_content_html(&document);

    // Now walk main content and extract blocks
    let main_doc = Html::parse_fragment(&main_html);
    let root = main_doc.root_element();

    let mut state = HtmlExtractState::new();
    walk_element(&root, &mut state);

    // Apply the first H1 as title if no <title> found, or if page title is empty
    let title = page_title
        .filter(|t| !t.is_empty())
        .or_else(|| state.first_h1.clone());

    Ok(ExtractionOutput {
        text: state.normalized,
        blocks: state.blocks,
        title,
    })
}

/// Extract the text content of the <title> element.
fn extract_page_title(document: &Html) -> Option<String> {
    let selector = Selector::parse("title").ok()?;
    let el = document.select(&selector).next()?;
    let text: String = el.text().collect::<Vec<_>>().join(" ");
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Extract the inner HTML of the main content element.
///
/// Priority: `<header>` + all `<article>` elements (concatenated) > `<main>` >
/// `[role="main"]` > `<body>`.
///
/// If one or more `<article>` elements are present, their inner HTML is
/// concatenated (optionally preceded by any `<header>` inner HTML found at the
/// top level).  This ensures every article is included, not just the first.
fn extract_main_content_html(document: &Html) -> String {
    // Collect all <article> elements.
    let article_sel = Selector::parse("article").unwrap();
    let articles: Vec<_> = document.select(&article_sel).collect();

    if !articles.is_empty() {
        // Optionally prepend top-level <header> content.
        let mut combined = String::new();

        if let Ok(header_sel) = Selector::parse("header") {
            if let Some(header_el) = document.select(&header_sel).next() {
                let header_html = header_el.inner_html();
                if !header_html.trim().is_empty() {
                    combined.push_str(&header_html);
                }
            }
        }

        for article in &articles {
            combined.push_str(&article.inner_html());
        }

        return combined;
    }

    // Fall through to <main>, [role="main"], <body>.
    let candidates = ["main", "[role=\"main\"]", "body"];

    for selector_str in &candidates {
        if let Ok(sel) = Selector::parse(selector_str) {
            if let Some(el) = document.select(&sel).next() {
                return el.inner_html();
            }
        }
    }

    // Fallback: entire document
    document.root_element().inner_html()
}

/// State accumulated while walking the HTML DOM.
struct HtmlExtractState {
    normalized: String,
    blocks: Vec<localdb_core::Block>,
    ordinal: usize,
    /// Current heading path, one entry per level (index 0 = H1).
    heading_levels: [Option<String>; 6],
    /// The first H1 text seen, for title fallback.
    first_h1: Option<String>,
}

impl HtmlExtractState {
    fn new() -> Self {
        Self {
            normalized: String::new(),
            blocks: Vec::new(),
            ordinal: 0,
            heading_levels: Default::default(),
            first_h1: None,
        }
    }

    fn current_heading_path(&self) -> Vec<String> {
        self.heading_levels
            .iter()
            .filter_map(|x| x.clone())
            .collect()
    }

    fn push_block(&mut self, kind: BlockKind, text: String, heading_path: Vec<String>) {
        if text.trim().is_empty() {
            return;
        }
        let start = self.normalized.len();
        self.normalized.push_str(&text);
        if !self.normalized.ends_with('\n') {
            self.normalized.push('\n');
        }
        let end = self.normalized.len();

        let block = make_block(
            self.ordinal,
            kind,
            text,
            Span::new(start, end),
            heading_path,
        );
        self.blocks.push(block);
        self.ordinal += 1;
    }
}

/// Tags we skip entirely (navigation, scripts, etc.)
fn should_skip_tag(tag: &str) -> bool {
    matches!(
        tag,
        "script"
            | "style"
            | "nav"
            | "footer"
            | "aside"
            | "noscript"
            | "iframe"
            | "form"
            | "button"
            | "input"
            | "select"
            | "textarea"
            | "meta"
            | "link"
    )
}

/// Walk an HTML element recursively and accumulate blocks.
fn walk_element(element: &scraper::ElementRef, state: &mut HtmlExtractState) {
    let tag = element.value().name().to_lowercase();

    if should_skip_tag(&tag) {
        return;
    }

    match tag.as_str() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag[1..].parse::<usize>().unwrap_or(1);
            let text = collect_text(element);
            let text = text.trim().to_string();
            if !text.is_empty() {
                let idx = level - 1;
                state.heading_levels[idx] = Some(text.clone());
                // Clear deeper levels
                for deeper in (idx + 1)..6 {
                    state.heading_levels[deeper] = None;
                }

                if level == 1 && state.first_h1.is_none() {
                    state.first_h1 = Some(text.clone());
                }

                let heading_path = state.current_heading_path();
                state.push_block(BlockKind::Heading, text, heading_path);
            }
        }

        "p" => {
            let text = collect_text(element).trim().to_string();
            let heading_path = state.current_heading_path();
            state.push_block(BlockKind::Paragraph, text, heading_path);
        }

        "pre" | "code" => {
            // Only treat as a standalone code block if it's a <pre> or a top-level <code>
            let text = collect_text(element).trim().to_string();
            if !text.is_empty() {
                let heading_path = state.current_heading_path();
                state.push_block(BlockKind::Code, text, heading_path);
            }
        }

        "blockquote" => {
            let text = collect_text(element).trim().to_string();
            if !text.is_empty() {
                let heading_path = state.current_heading_path();
                state.push_block(BlockKind::Blockquote, text, heading_path);
            }
        }

        "ul" | "ol" => {
            // Walk list items
            if let Ok(li_sel) = Selector::parse("li") {
                for li in element.select(&li_sel) {
                    // Only direct children
                    let text = collect_text(&li).trim().to_string();
                    if !text.is_empty() {
                        let heading_path = state.current_heading_path();
                        state.push_block(BlockKind::ListItem, text, heading_path);
                    }
                }
            }
        }

        "li" => {
            // Already handled by ul/ol above; skip to avoid double-counting
        }

        "table" => {
            let text = collect_text(element).trim().to_string();
            if !text.is_empty() {
                let heading_path = state.current_heading_path();
                state.push_block(BlockKind::Table, text, heading_path);
            }
        }

        "div" | "section" | "article" | "main" | "header" | "figure" | "details" | "summary"
        | "address" | "body" | "html" => {
            // Container elements — recurse into children
            for child in element.children() {
                if let Some(child_el) = scraper::ElementRef::wrap(child) {
                    walk_element(&child_el, state);
                }
            }
        }

        // Inline elements and others — skip (content handled by parent)
        _ => {
            // For unrecognized block-level elements, still recurse
            for child in element.children() {
                if let Some(child_el) = scraper::ElementRef::wrap(child) {
                    walk_element(&child_el, state);
                }
            }
        }
    }
}

/// Collect all text content from an element and its descendants.
fn collect_text(element: &scraper::ElementRef) -> String {
    element.text().collect::<Vec<_>>().join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::BlockKind;

    #[test]
    fn extracts_title_from_title_tag() {
        let html = "<html><head><title>My Page</title></head><body><p>Content</p></body></html>";
        let out = extract_html(html).unwrap();
        assert_eq!(out.title, Some("My Page".to_string()));
    }

    #[test]
    fn extracts_title_from_h1_when_no_title_tag() {
        let html = "<html><body><h1>My Article</h1><p>Content</p></body></html>";
        let out = extract_html(html).unwrap();
        assert_eq!(out.title, Some("My Article".to_string()));
    }

    #[test]
    fn extracts_headings_with_correct_kind() {
        let html = "<body><h1>Title</h1><h2>Section</h2><p>Content</p></body>";
        let out = extract_html(html).unwrap();
        let headings: Vec<_> = out
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Heading)
            .collect();
        assert_eq!(headings.len(), 2, "Expected 2 headings");
    }

    #[test]
    fn heading_paths_correct_for_nested_headings() {
        let html = "<body><h1>Main</h1><h2>Sub</h2><h3>Sub-sub</h3><p>Deep.</p></body>";
        let out = extract_html(html).unwrap();

        let deep_para = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph && b.text.contains("Deep"))
            .expect("Expected deep paragraph");
        assert_eq!(
            deep_para.heading_path,
            vec!["Main", "Sub", "Sub-sub"],
            "Paragraph should have full heading path"
        );
    }

    #[test]
    fn prefers_article_content() {
        let html = r#"
            <html><body>
                <nav>Navigation - should be excluded</nav>
                <article>
                    <h1>Article Title</h1>
                    <p>Article content.</p>
                </article>
                <footer>Footer - should be excluded</footer>
            </body></html>
        "#;
        let out = extract_html(html).unwrap();

        // Navigation and footer should not be in blocks
        let nav_block = out.blocks.iter().find(|b| b.text.contains("Navigation"));
        assert!(
            nav_block.is_none(),
            "Navigation should be excluded, but found: {:?}",
            nav_block
        );

        // Article content should be present
        assert!(
            out.blocks
                .iter()
                .any(|b| b.text.contains("Article content")),
            "Article content should be present"
        );
    }

    #[test]
    fn spans_index_into_normalized_text_exactly() {
        let html = "<body><h1>Title</h1><p>Paragraph one.</p><p>Paragraph two.</p></body>";
        let out = extract_html(html).unwrap();

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
                "Span should contain block text: {:?} not found in {:?}",
                block.text,
                span_text
            );
        }
    }

    #[test]
    fn heading_path_resets_when_parent_changes() {
        let html =
            "<body><h1>H1a</h1><h2>H2a</h2><p>Under H2a</p><h2>H2b</h2><p>Under H2b</p></body>";
        let out = extract_html(html).unwrap();

        let under_h2b = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph && b.text.contains("Under H2b"))
            .expect("Expected 'Under H2b' paragraph");
        assert_eq!(under_h2b.heading_path, vec!["H1a", "H2b"]);
    }

    #[test]
    fn html_extracts_all_articles_and_header() {
        let html = r#"<html><body>
          <header><h1>Site Title</h1><p>Tagline</p></header>
          <article><h2>First Article</h2><p>First content.</p></article>
          <article><h2>Second Article</h2><p>Second content.</p></article>
        </body></html>"#;
        let out = extract_html(html).unwrap();

        // Header content should be included
        assert!(
            out.blocks
                .iter()
                .any(|b| b.text.contains("Site Title") || b.text.contains("Tagline")),
            "header content should be extracted, got blocks: {:?}",
            out.blocks.iter().map(|b| &b.text).collect::<Vec<_>>()
        );

        // Both articles should be present
        assert!(
            out.blocks.iter().any(|b| b.text.contains("First content")),
            "first article should be extracted"
        );
        assert!(
            out.blocks.iter().any(|b| b.text.contains("Second content")),
            "second article should be extracted"
        );
    }

    #[test]
    fn golden_file_article_html() {
        let html = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/article.html"
        ))
        .expect("fixture file not found");
        let out = extract_html(&html).unwrap();

        // Title should be extracted
        assert!(out.title.is_some(), "Expected a title");

        // Should have headings and paragraphs
        let headings: Vec<_> = out
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Heading)
            .collect();
        assert!(!headings.is_empty(), "Expected at least one heading");

        // All spans valid
        for block in &out.blocks {
            assert!(
                block.span.end <= out.text.len(),
                "Span end {} exceeds text length {}",
                block.span.end,
                out.text.len()
            );
        }

        // The fixture has a "Section One" heading, check it's in blocks
        let section_one = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Heading && b.text.contains("Section One"));
        assert!(section_one.is_some(), "Expected 'Section One' heading");
    }

    #[test]
    fn section_one_has_correct_heading_path_in_fixture() {
        let html = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/article.html"
        ))
        .expect("fixture file not found");
        let out = extract_html(&html).unwrap();

        // "Section One" is H2 under H1 "Main Article Title"
        let section_one = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Heading && b.text.contains("Section One"))
            .expect("Expected Section One heading");
        assert!(
            section_one
                .heading_path
                .contains(&"Main Article Title".to_string()),
            "Section One should be under Main Article Title, got {:?}",
            section_one.heading_path
        );
    }

    #[test]
    fn subsection_a_has_three_level_path() {
        let html = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/article.html"
        ))
        .expect("fixture file not found");
        let out = extract_html(&html).unwrap();

        // "Subsection A" is H3 under H2 "Section One" under H1 "Main Article Title"
        let subsection = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Heading && b.text.contains("Subsection A"))
            .expect("Expected Subsection A heading");
        assert_eq!(
            subsection.heading_path,
            vec!["Main Article Title", "Section One", "Subsection A"]
        );
    }
}
