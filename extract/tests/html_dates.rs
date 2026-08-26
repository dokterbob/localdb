//! `extract::html::extract_html_date` precedence tests (issue #251).
//!
//! Sibling integration test file per repo convention (#213) — new tests for
//! `extract::html` additions live here rather than growing that module's own
//! `#[cfg(test)] mod tests` further.

use extract::html::extract_html_date;

#[test]
fn html_json_ld_date_wins_over_dcterms_meta() {
    let html = r#"<html><head>
        <script type="application/ld+json">
        {"@type": "Article", "datePublished": "2023-05-01"}
        </script>
        <meta name="dcterms.date" content="2022-01-01">
        <meta property="article:published_time" content="2021-06-15">
    </head><body><p>Content</p></body></html>"#;

    let (date, source) = extract_html_date(html).expect("expected a date");
    assert_eq!(date, "2023-05-01");
    assert_eq!(source, "html-json-ld");
}

#[test]
fn html_json_ld_graph_descends_into_article_node() {
    let html = r#"<html><head>
        <script type="application/ld+json">
        {
          "@context": "https://schema.org",
          "@graph": [
            {"@type": "WebSite", "name": "Example"},
            {"@type": "BreadcrumbList"},
            {"@type": "Article", "datePublished": "2020-09-10"}
          ]
        }
        </script>
    </head><body><p>Content</p></body></html>"#;

    let (date, source) = extract_html_date(html).expect("expected a date");
    assert_eq!(date, "2020-09-10");
    assert_eq!(source, "html-json-ld");
}

#[test]
fn html_dcterms_meta_used_when_no_json_ld() {
    let html = r#"<html><head>
        <meta name="dcterms.date" content="2024-02-20">
    </head><body><p>Content</p></body></html>"#;

    let (date, source) = extract_html_date(html).expect("expected a date");
    assert_eq!(date, "2024-02-20");
    assert_eq!(source, "html-meta");
}

#[test]
fn html_article_published_time_used_when_no_dcterms() {
    let html = r#"<html><head>
        <meta property="article:published_time" content="2024-02-20">
    </head><body><p>Content</p></body></html>"#;

    let (date, source) = extract_html_date(html).expect("expected a date");
    assert_eq!(date, "2024-02-20");
    assert_eq!(source, "html-meta");
}

#[test]
fn html_legacy_meta_date_used_as_last_resort() {
    let html = r#"<html><head>
        <meta name="date" content="2015-07-04">
    </head><body><p>Content</p></body></html>"#;

    let (date, source) = extract_html_date(html).expect("expected a date");
    assert_eq!(date, "2015-07-04");
    assert_eq!(source, "html-meta");
}

#[test]
fn html_no_date_signal_leaves_date_none() {
    let html = "<html><head><title>No Dates</title></head><body><p>Content</p></body></html>";
    assert_eq!(extract_html_date(html), None);
}

#[test]
fn malformed_json_ld_skips_to_meta() {
    // Trailing commas after the author name and after the author object —
    // a realistic CMS templating bug. `serde_json` fails to parse it; the
    // extractor must fall through to the meta tag, not silently return None.
    let html = r#"<html><head>
        <script type="application/ld+json">
        {"@type": "Article", "author": {"name": "Jane",}, }
        </script>
        <meta property="article:published_time" content="2019-11-11">
    </head><body><p>Content</p></body></html>"#;

    let (date, source) = extract_html_date(html).expect("expected a date");
    assert_eq!(date, "2019-11-11");
    assert_eq!(source, "html-meta");
}
