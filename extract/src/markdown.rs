//! Markdown extraction: passthrough with title detection.
//!
//! Markdown is the universal normalized representation — the input bytes ARE the
//! output markdown. Title is extracted from the first H1 via pulldown-cmark.

use localdb_core::Error;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Extract a Markdown document.
///
/// Returns `(markdown, title)` where `markdown` is the raw UTF-8 input (CRLF
/// normalized to LF) and `title` is the text of the first H1 heading, if any.
pub fn extract_markdown(input: &str) -> Result<(String, Option<String>), Error> {
    let markdown = input.replace("\r\n", "\n").replace('\r', "\n");
    let title = extract_first_h1(&markdown);
    Ok((markdown, title))
}

/// Scan a Markdown document's YAML front-matter for a top-level `date:` key
/// (issue #251).
///
/// A second, independent read of the front-matter body via
/// `localdb_core::markdown_blocks::extract_frontmatter` — that function only
/// carves the front-matter block out for later stripping; this scans the
/// same body for the `date` key without changing that carving at all.
///
/// Deliberately narrow (matching the front-matter carving's own YAML
/// tolerance, not a general YAML parser): only a line starting with
/// literal `date:` (no leading whitespace — nested keys are out of scope,
/// #195), remainder trimmed, one pair of matching `'…'`/`"…"` quotes
/// stripped if present. Both `date: 2020-11-05` (bare) and
/// `date: "2020-11-05"` (quoted) — both present in the wild — normalize to
/// the same raw string. Multi-doc front matter and folded scalars are out
/// of scope (#195). Returns the raw claim as-is (`date_original`
/// convention) — no date parsing here.
pub(crate) fn extract_frontmatter_date(input: &str) -> Option<String> {
    let (frontmatter, _) = localdb_core::markdown_blocks::extract_frontmatter(input);
    let body = frontmatter?;

    for line in body.lines() {
        let Some(rest) = line.strip_prefix("date:") else {
            continue;
        };
        let value = rest.trim();
        if value.is_empty() {
            continue;
        }
        let unquoted = strip_matching_quotes(value);
        if !unquoted.is_empty() {
            return Some(unquoted.to_string());
        }
    }
    None
}

/// Strip one pair of matching leading/trailing `'` or `"` quotes, if present.
fn strip_matching_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'\'' || first == b'"') && first == last {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Extract the text of the first H1 heading using pulldown-cmark's offset iterator.
fn extract_first_h1(markdown: &str) -> Option<String> {
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
    fn passthrough_preserves_markdown() {
        let md =
            "# Title\n\nSome paragraph with **bold** and `code`.\n\n```rust\nfn main() {}\n```\n";
        let (out, _) = extract_markdown(md).unwrap();
        assert_eq!(out, md, "passthrough must return input unchanged");
    }

    #[test]
    fn extracts_title_from_h1() {
        let md = "# My Title\n\nSome paragraph text.\n";
        let (_, title) = extract_markdown(md).unwrap();
        assert_eq!(title, Some("My Title".to_string()));
    }

    #[test]
    fn no_title_when_no_h1() {
        let md = "## Just H2\n\nNo H1 here.\n";
        let (_, title) = extract_markdown(md).unwrap();
        assert!(title.is_none(), "should be None when there is no H1");
    }

    #[test]
    fn first_h1_only() {
        let md = "# First\n\n# Second\n";
        let (_, title) = extract_markdown(md).unwrap();
        assert_eq!(title, Some("First".to_string()));
    }

    #[test]
    fn crlf_normalized_to_lf() {
        let md = "# Title\r\n\r\nParagraph.\r\n";
        let (out, _) = extract_markdown(md).unwrap();
        assert!(!out.contains('\r'), "CRLF should be normalized to LF");
    }

    #[test]
    fn markdown_headings_preserved_in_output() {
        let md = "# Section\n\nContent.\n";
        let (out, _) = extract_markdown(md).unwrap();
        assert!(
            out.contains("# Section"),
            "heading markers must be preserved in output"
        );
    }

    #[test]
    fn code_fence_preserved() {
        let md = "```python\nprint('hello')\n```\n";
        let (out, _) = extract_markdown(md).unwrap();
        assert!(out.contains("```python"), "code fences must be preserved");
    }
}
