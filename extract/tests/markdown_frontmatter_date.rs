//! `MarkdownParser` front-matter `date:` extraction tests (issue #251).
//!
//! Sibling integration test file per repo convention (#213) — exercises the
//! public parser surface end-to-end rather than the crate-private
//! `extract::markdown::extract_frontmatter_date` helper directly.

use extract::parsers::markdown::MarkdownParser;
use localdb_core::parser::{Parser, Probe};

#[test]
fn markdown_frontmatter_date_key_extracted() {
    // Bare/unquoted YAML scalar — the common case.
    let md = b"---\ntitle: Post\ndate: 2020-11-05\n---\n\n# Heading\n\nBody.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(doc.metadata.date.as_deref(), Some("2020-11-05"));
    assert_eq!(doc.metadata.date_source.as_deref(), Some("front-matter"));
}

#[test]
fn markdown_frontmatter_date_key_extracted_quoted() {
    // Quoted string — must normalize to the identical raw value as the bare
    // form, not carry the quotes through.
    let md = b"---\ntitle: Post\ndate: \"2020-11-05\"\n---\n\n# Heading\n\nBody.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(doc.metadata.date.as_deref(), Some("2020-11-05"));
    assert_eq!(doc.metadata.date_source.as_deref(), Some("front-matter"));
}

#[test]
fn markdown_frontmatter_single_quoted_date_extracted() {
    let md = b"---\ndate: '2020-11-05'\n---\n\n# Heading\n\nBody.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(doc.metadata.date.as_deref(), Some("2020-11-05"));
}

#[test]
fn markdown_no_date_key_leaves_date_none() {
    let md = b"---\ntitle: Post\n---\n\n# Heading\n\nBody.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(doc.metadata.date, None);
    assert_eq!(doc.metadata.date_source, None);
}

#[test]
fn markdown_no_frontmatter_leaves_date_none() {
    let md = b"# Heading\n\nBody with no front matter at all.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(doc.metadata.date, None);
    assert_eq!(doc.metadata.date_source, None);
}

#[test]
fn markdown_frontmatter_date_bare_value_strips_trailing_comment() {
    // A '#' preceded by whitespace on a bare scalar starts a comment.
    let md = b"---\ndate: 2020-11-05 # published\n---\n\n# Heading\n\nBody.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(doc.metadata.date.as_deref(), Some("2020-11-05"));
}

#[test]
fn markdown_frontmatter_date_quoted_value_strips_trailing_comment() {
    // Everything after the closing quote, including a '#' comment, is discarded.
    let md = b"---\ndate: \"2020-11-05\" # published\n---\n\n# Heading\n\nBody.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(doc.metadata.date.as_deref(), Some("2020-11-05"));
}

#[test]
fn markdown_frontmatter_date_quoted_value_preserves_hash_inside_quotes() {
    // A '#' inside the quotes is literal content, not a comment marker.
    let md = b"---\ndate: \"2020-11-05#not-a-comment\"\n---\n\n# Heading\n\nBody.\n";
    let probe = Probe::new(md, Some("post.md"), None);
    let doc = MarkdownParser.parse(&probe).unwrap().unwrap();
    assert_eq!(
        doc.metadata.date.as_deref(),
        Some("2020-11-05#not-a-comment")
    );
}
