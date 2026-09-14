//! HTML extraction with readability-style main-content selection.
//!
//! The readability selection (prefer `<article>`, `<main>`, `[role="main"]`)
//! is retained from the previous implementation. The selected HTML fragment is
//! then converted to Markdown via `anytomd`, replacing the hand-rolled DOM-walk
//! Block emitter.
//!
//! Title is extracted from `<title>` first, then falls back to the first `<h1>`
//! in the converted Markdown (via pulldown-cmark on the output).

use anytomd::{convert_bytes, ConversionOptions};
use localdb_core::Error;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use scraper::{Html, Selector};

/// Extract an HTML document.
///
/// Returns `(markdown, title)`. The main content is selected with a readability
/// heuristic; nav/footer/aside/script/style elements are excluded.
pub fn extract_html(input: &str) -> Result<(String, Option<String>), Error> {
    let document = Html::parse_document(input);

    let page_title = extract_page_title(&document);
    let main_html = extract_main_content_html(&document);

    let markdown = html_to_markdown(&main_html)?;

    let title = page_title
        .filter(|t| !t.is_empty())
        .or_else(|| extract_first_h1_from_markdown(&markdown));

    Ok((markdown, title))
}

/// Convert an HTML fragment to Markdown via anytomd's HTML converter.
fn html_to_markdown(html: &str) -> Result<String, Error> {
    let opts = ConversionOptions::default();
    match convert_bytes(html.as_bytes(), "html", &opts) {
        Ok(result) => Ok(result.markdown),
        Err(e) => Err(Error::ExtractionFailed {
            format: "html".into(),
            reason: e.to_string(),
        }),
    }
}

/// Convert a full XHTML/HTML document or fragment to Markdown **without**
/// readability main-content selection.
///
/// `extract_html` prunes to the main content (`<article>`/`<main>`), which is
/// correct for web pages but over-strips simple EPUB chapter XHTML, where the
/// whole `<body>` is the content. This sibling converts the bytes verbatim.
/// Keeping it here ensures all HTML→Markdown conversion lives in one module.
pub(crate) fn xhtml_to_markdown(bytes: &[u8]) -> Result<String, Error> {
    let opts = ConversionOptions::default();
    match convert_bytes(bytes, "html", &opts) {
        Ok(result) => Ok(result.markdown),
        Err(e) => Err(Error::ExtractionFailed {
            format: "xhtml".into(),
            reason: e.to_string(),
        }),
    }
}

/// Extract the text content of the `<title>` element.
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
/// Priority: `<article>` elements (concatenated, optionally preceded by
/// `<header>`) > `<main>` > `[role="main"]` > `<body>`.
fn extract_main_content_html(document: &Html) -> String {
    let article_sel = Selector::parse("article").unwrap();
    let articles: Vec<_> = document.select(&article_sel).collect();

    if !articles.is_empty() {
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

    let candidates = ["main", "[role=\"main\"]", "body"];
    for selector_str in &candidates {
        if let Ok(sel) = Selector::parse(selector_str) {
            if let Some(el) = document.select(&sel).next() {
                return el.inner_html();
            }
        }
    }

    document.root_element().inner_html()
}

/// Extract a document-level date claim from HTML (issue #251), in
/// precedence order (first hit wins):
///
/// 1. JSON-LD (`script[type="application/ld+json"]`, document order): the
///    first entry — the top-level object, or each member of an `@graph`
///    array in order — carrying `datePublished` (else `dateModified`). No
///    `@type` filtering: deterministic and simplest. A script whose content
///    fails to parse as JSON is skipped, not fatal — the next script (or the
///    meta-tag fallback) still gets a chance.
/// 2. `<meta name="dcterms.date" content="...">`
/// 3. `<meta property="article:published_time" content="...">`
/// 4. `<meta name="date" content="...">` (the oldest/legacy convention)
///
/// Returns the raw `content`/JSON-string value, trimmed — never date-parsed
/// here (`date_original` convention; normalization happens downstream) —
/// paired with which signal won, for `DublinCoreMetadata::date_source`.
pub fn extract_html_date(input: &str) -> Option<(String, &'static str)> {
    let document = Html::parse_document(input);

    if let Some(date) = extract_json_ld_date(&document) {
        return Some((date, "html-json-ld"));
    }

    for selector_str in [
        r#"meta[name="dcterms.date"]"#,
        r#"meta[property="article:published_time"]"#,
        r#"meta[name="date"]"#,
    ] {
        if let Some(date) = extract_meta_content(&document, selector_str) {
            return Some((date, "html-meta"));
        }
    }

    None
}

/// `<meta ... content="...">`, trimmed; scans every element matching
/// `selector_str` in document order and returns the first one with a
/// non-empty `content` attribute. `None` if the selector matches nothing, or
/// every match has no `content` attribute or an empty one.
fn extract_meta_content(document: &Html, selector_str: &str) -> Option<String> {
    let selector = Selector::parse(selector_str).ok()?;
    document.select(&selector).find_map(|el| {
        let content = el.value().attr("content")?.trim();
        (!content.is_empty()).then(|| content.to_string())
    })
}

/// Scan every `script[type="application/ld+json"]` in document order for
/// the first `datePublished`/`dateModified` claim, descending into `@graph`
/// when the top-level object itself carries none.
fn extract_json_ld_date(document: &Html) -> Option<String> {
    let selector = Selector::parse(r#"script[type="application/ld+json"]"#).ok()?;
    for el in document.select(&selector) {
        let text: String = el.text().collect();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue; // malformed JSON in this block — try the next script
        };
        if let Some(date) = json_ld_date_from_value(&value) {
            return Some(date);
        }
    }
    None
}

/// `datePublished`/`dateModified` from a top-level JSON-LD value, or (when
/// absent there) from the first member of a top-level array or `@graph`
/// array that carries either field.
fn json_ld_date_from_value(value: &serde_json::Value) -> Option<String> {
    if let Some(date) = json_ld_date_from_object(value) {
        return Some(date);
    }
    if let Some(arr) = value.as_array() {
        if let Some(date) = find_date_in_array(arr) {
            return Some(date);
        }
    }
    value
        .get("@graph")
        .and_then(|g| g.as_array())
        .and_then(|graph| find_date_in_array(graph))
}

/// First element in `arr` (document order) carrying a `datePublished`/
/// `dateModified` claim. Shared by top-level JSON-LD arrays and `@graph`.
fn find_date_in_array(arr: &[serde_json::Value]) -> Option<String> {
    arr.iter().find_map(json_ld_date_from_object)
}

/// `datePublished`, falling back to `dateModified`, from a single JSON-LD
/// node (no `@type` filtering).
fn json_ld_date_from_object(value: &serde_json::Value) -> Option<String> {
    value
        .get("datePublished")
        .or_else(|| value.get("dateModified"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract the first H1 text from a Markdown string (for title fallback).
fn extract_first_h1_from_markdown(markdown: &str) -> Option<String> {
    let parser = Parser::new_ext(markdown, Options::empty());
    let mut in_h1 = false;
    let mut buf = String::new();

    for event in parser {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1,
                ..
            }) => {
                in_h1 = true;
                buf.clear();
            }
            Event::Text(t) if in_h1 => buf.push_str(&t),
            Event::End(TagEnd::Heading(_)) if in_h1 => {
                let title = buf.trim().to_string();
                if !title.is_empty() {
                    return Some(title);
                }
                break;
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_from_title_tag() {
        let html = "<html><head><title>My Page</title></head><body><p>Content</p></body></html>";
        let (_, title) = extract_html(html).unwrap();
        assert_eq!(title, Some("My Page".to_string()));
    }

    #[test]
    fn extracts_title_from_h1_when_no_title_tag() {
        let html = "<html><body><h1>My Article</h1><p>Content</p></body></html>";
        let (_, title) = extract_html(html).unwrap();
        assert_eq!(title, Some("My Article".to_string()));
    }

    #[test]
    fn markdown_contains_headings() {
        let html = "<body><h1>Title</h1><h2>Section</h2><p>Content</p></body>";
        let (md, _) = extract_html(html).unwrap();
        assert!(
            md.contains("Title") || md.contains('#'),
            "should have heading content"
        );
    }

    #[test]
    fn readability_excludes_nav_and_footer() {
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
        let (md, _) = extract_html(html).unwrap();
        assert!(
            !md.contains("Navigation - should be excluded"),
            "Navigation should be excluded"
        );
        assert!(
            md.contains("Article content"),
            "Article content should be present"
        );
    }

    #[test]
    fn markdown_contains_article_content() {
        let html = "<html><body><article><h2>Post</h2><p>The text.</p></article></body></html>";
        let (md, _) = extract_html(html).unwrap();
        assert!(
            md.contains("The text"),
            "article content must be in markdown"
        );
    }

    #[test]
    fn html_extracts_all_articles_and_header() {
        let html = r#"<html><body>
          <header><h1>Site Title</h1><p>Tagline</p></header>
          <article><h2>First Article</h2><p>First content.</p></article>
          <article><h2>Second Article</h2><p>Second content.</p></article>
        </body></html>"#;
        let (md, _) = extract_html(html).unwrap();
        assert!(
            md.contains("First content") || md.contains("First Article"),
            "first article should be extracted"
        );
        assert!(
            md.contains("Second content") || md.contains("Second Article"),
            "second article should be extracted"
        );
    }

    #[test]
    fn xhtml_to_markdown_keeps_full_body_no_readability_stripping() {
        // A page where readability WOULD drop the <nav>; xhtml_to_markdown must keep it.
        let xhtml = br#"<html><body>
            <nav>Sidebar link</nav>
            <h1>Chapter Title</h1>
            <p>Body paragraph text.</p>
        </body></html>"#;
        let md = xhtml_to_markdown(xhtml).unwrap();
        assert!(
            md.contains("Body paragraph text"),
            "body content must be present: {md}"
        );
        assert!(
            md.contains("Sidebar link"),
            "no readability pruning — nav content must be retained: {md}"
        );
    }

    #[test]
    fn golden_file_article_html() {
        let html = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/article.html"
        ))
        .expect("fixture file not found");
        let (md, title) = extract_html(&html).unwrap();

        assert!(title.is_some(), "Expected a title from fixture");
        assert!(!md.is_empty(), "Expected non-empty markdown from fixture");

        // The fixture has a "Section One" heading; it should appear in the markdown.
        assert!(
            md.contains("Section One"),
            "Expected 'Section One' in markdown output, got: {}",
            &md[..md.len().min(500)]
        );
    }
}
