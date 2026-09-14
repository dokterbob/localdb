//! Corpus-driven title/date/date_source extraction tests (issue #251).
//!
//! Drives the real parser chain (`extract::parsers::*`) over the fixtures
//! copied from the validation corpus into `tests/fixtures/metadata/`.
//! Assertions follow the corpus `MANIFEST.md`'s expected outcomes under
//! strict explicit-over-heuristic precedence.

use extract::parsers::html::HtmlParser;
use extract::parsers::markdown::MarkdownParser;
use extract::parsers::office::OfficeParser;
use localdb_core::parser::{Parser, Probe};

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/metadata")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read fixture {name}: {e}"))
}

// ---------------------------------------------------------------------------
// Office (docx/pptx) — docProps/core.xml vs. body heuristic
// ---------------------------------------------------------------------------

#[test]
fn docx_real_title_metadata_wins_over_h1() {
    let bytes = fixture_bytes("docx-real-title.docx");
    let probe = Probe::new(&bytes, Some("docx-real-title.docx"), None);
    let doc = OfficeParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.title.as_deref(), Some("Q3 Board Report"));
    assert_eq!(doc.metadata.title.as_deref(), Some("Q3 Board Report"));
    assert_eq!(doc.metadata.date.as_deref(), Some("2019-03-04T00:00:00Z"));
    assert_eq!(
        doc.metadata.date_source.as_deref(),
        Some("office-core-properties")
    );
}

#[test]
fn docx_junk_title_explicit_still_wins_over_h1() {
    // "Document1" is Word's stale default placeholder title. Under strict
    // explicit-over-heuristic precedence this still wins over the H1
    // ("Annual Safety Review 2021") — a quality-aware precedence rule that
    // preferred the heading instead was considered and deliberately not
    // built; the corpus scout found no widespread junk-title problem in
    // practice. This test documents that tradeoff, not just pins behavior.
    let bytes = fixture_bytes("docx-junk-title.docx");
    let probe = Probe::new(&bytes, Some("docx-junk-title.docx"), None);
    let doc = OfficeParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.title.as_deref(), Some("Document1"));
    assert_eq!(doc.metadata.date.as_deref(), Some("2021-09-12T00:00:00Z"));
    assert_eq!(
        doc.metadata.date_source.as_deref(),
        Some("office-core-properties")
    );
}

#[test]
fn docx_empty_title_falls_through_to_h1() {
    // `<dc:title/>` is present-but-empty — treated as absent, so the title
    // falls through to anytomd's H1-derived title.
    let bytes = fixture_bytes("docx-empty-title.docx");
    let probe = Probe::new(&bytes, Some("docx-empty-title.docx"), None);
    let doc = OfficeParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.title.as_deref(), Some("Migration Runbook"));
    assert_eq!(doc.metadata.date.as_deref(), Some("2020-06-01T00:00:00Z"));
    assert_eq!(
        doc.metadata.date_source.as_deref(),
        Some("office-core-properties")
    );
}

#[test]
fn docx_no_coreprops_falls_through_cleanly() {
    // docProps/core.xml (and its [Content_Types].xml override / _rels/.rels
    // relationship) is entirely absent from the zip — must not crash, and
    // both title and date fall through to pure content heuristics (none for
    // date; there is no heuristic date source).
    let bytes = fixture_bytes("docx-no-coreprops.docx");
    let probe = Probe::new(&bytes, Some("docx-no-coreprops.docx"), None);
    let doc = OfficeParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.title.as_deref(), Some("Orphan Doc"));
    assert_eq!(doc.metadata.date, None);
    assert_eq!(doc.metadata.date_source, None);
}

#[test]
fn docx_no_h1_metadata_still_used() {
    // The body has no heading styles at all — proves metadata is read and
    // used even when the heuristic side has no candidate whatsoever.
    let bytes = fixture_bytes("docx-no-h1.docx");
    let probe = Probe::new(&bytes, Some("docx-no-h1.docx"), None);
    let doc = OfficeParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.title.as_deref(), Some("Only Metadata Knows"));
    assert_eq!(doc.metadata.date.as_deref(), Some("2022-04-17T00:00:00Z"));
    assert_eq!(
        doc.metadata.date_source.as_deref(),
        Some("office-core-properties")
    );
}

#[test]
fn pptx_title_slide_metadata_and_placeholder_agree() {
    let bytes = fixture_bytes("pptx-title-slide.pptx");
    let probe = Probe::new(&bytes, Some("pptx-title-slide.pptx"), None);
    let doc = OfficeParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.title.as_deref(), Some("Quarterly Roadmap Review"));
    assert_eq!(doc.metadata.date.as_deref(), Some("2023-08-14T00:00:00Z"));
    assert_eq!(
        doc.metadata.date_source.as_deref(),
        Some("office-core-properties")
    );
}

// ---------------------------------------------------------------------------
// HTML — JSON-LD / meta tag precedence
// ---------------------------------------------------------------------------

#[test]
fn html_jsonld_and_meta_json_ld_wins() {
    let bytes = fixture_bytes("html-jsonld-and-meta.html");
    let probe = Probe::new(&bytes, Some("html-jsonld-and-meta.html"), None);
    let doc = HtmlParser.parse(&probe).unwrap().unwrap();

    assert_eq!(
        doc.metadata.date.as_deref(),
        Some("2023-05-01T12:00:00Z"),
        "JSON-LD datePublished must win over both meta-tag conventions"
    );
    assert_eq!(doc.metadata.date_source.as_deref(), Some("html-json-ld"));
}

#[test]
fn html_jsonld_graph_descends_to_article_node() {
    let bytes = fixture_bytes("html-jsonld-graph.html");
    let probe = Probe::new(&bytes, Some("html-jsonld-graph.html"), None);
    let doc = HtmlParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.metadata.date.as_deref(), Some("2020-09-10T00:00:00Z"));
    assert_eq!(doc.metadata.date_source.as_deref(), Some("html-json-ld"));
}

#[test]
fn html_meta_only_uses_article_published_time() {
    let bytes = fixture_bytes("html-meta-only.html");
    let probe = Probe::new(&bytes, Some("html-meta-only.html"), None);
    let doc = HtmlParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.metadata.date.as_deref(), Some("2024-02-20T14:00:00Z"));
    assert_eq!(doc.metadata.date_source.as_deref(), Some("html-meta"));
}

#[test]
fn html_legacy_meta_date_reachable() {
    let bytes = fixture_bytes("html-legacy-meta.html");
    let probe = Probe::new(&bytes, Some("html-legacy-meta.html"), None);
    let doc = HtmlParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.metadata.date.as_deref(), Some("2015-07-04"));
    assert_eq!(doc.metadata.date_source.as_deref(), Some("html-meta"));
}

#[test]
fn html_no_dates_leaves_date_none() {
    let bytes = fixture_bytes("html-no-dates.html");
    let probe = Probe::new(&bytes, Some("html-no-dates.html"), None);
    let doc = HtmlParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.metadata.date, None);
    assert_eq!(doc.metadata.date_source, None);
}

#[test]
fn html_malformed_jsonld_falls_back_to_meta() {
    let bytes = fixture_bytes("html-jsonld-malformed.html");
    let probe = Probe::new(&bytes, Some("html-jsonld-malformed.html"), None);
    let doc = HtmlParser.parse(&probe).unwrap().unwrap();

    assert_eq!(
        doc.metadata.date.as_deref(),
        Some("2019-11-11T00:00:00Z"),
        "malformed JSON-LD must fail safely and fall through to the meta tag"
    );
    assert_eq!(doc.metadata.date_source.as_deref(), Some("html-meta"));
}

// ---------------------------------------------------------------------------
// Markdown — front-matter date
// ---------------------------------------------------------------------------

#[test]
fn md_frontmatter_bare_date_extracted() {
    let bytes = fixture_bytes("md-frontmatter-date.md");
    let probe = Probe::new(&bytes, Some("md-frontmatter-date.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.metadata.date.as_deref(), Some("2020-11-05"));
    assert_eq!(doc.metadata.date_source.as_deref(), Some("front-matter"));
}

#[test]
fn md_frontmatter_quoted_date_normalizes_same_as_bare() {
    let bytes = fixture_bytes("md-frontmatter-quoted-date.md");
    let probe = Probe::new(&bytes, Some("md-frontmatter-quoted-date.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();

    assert_eq!(doc.metadata.date.as_deref(), Some("2020-11-05"));
    assert_eq!(doc.metadata.date_source.as_deref(), Some("front-matter"));
}

#[test]
fn md_no_frontmatter_falls_through_to_h1_title_no_date() {
    let bytes = fixture_bytes("md-no-frontmatter.md");
    let probe = Probe::new(&bytes, Some("md-no-frontmatter.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();

    assert!(doc.title.is_some(), "must fall through to the first H1");
    assert_eq!(doc.metadata.date, None);
    assert_eq!(doc.metadata.date_source, None);
}
