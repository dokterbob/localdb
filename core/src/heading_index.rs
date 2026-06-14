//! Heading index for Markdown documents.
//!
//! Replaces the Block-based heading-path attribution used by the old extractor.
//! `build_heading_index` scans real Markdown with `pulldown-cmark` and returns
//! a sorted `(byte_offset, heading_path)` vec; `heading_path_at` binary-searches
//! it to find the path at any given chunk start offset.

use pulldown_cmark::{Event, HeadingLevel, Options, Tag, TagEnd};

/// Build a heading index from Markdown text.
///
/// Returns a vec of `(byte_offset, heading_path)` entries sorted by offset.
/// Each entry represents the heading context that applies *from* that offset
/// (i.e. the heading whose open tag started there).
///
/// The index is cheap to build and is binary-searched per chunk in the chunker.
pub fn build_heading_index(markdown: &str) -> Vec<(usize, Vec<String>)> {
    let mut heading_levels: [Option<String>; 6] = Default::default();
    let mut result = Vec::new();

    let parser = pulldown_cmark::Parser::new_ext(markdown, Options::empty()).into_offset_iter();

    let mut in_heading: Option<(u8, usize)> = None;
    let mut text_buf = String::new();

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                text_buf.clear();
                in_heading = Some((heading_level_to_u8(level), range.start));
            }
            Event::Text(t) if in_heading.is_some() => {
                text_buf.push_str(&t);
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((lvl, offset)) = in_heading.take() {
                    let heading_text = std::mem::take(&mut text_buf);
                    let idx = (lvl - 1) as usize;
                    heading_levels[idx] = Some(heading_text);
                    for deeper in &mut heading_levels[idx + 1..] {
                        *deeper = None;
                    }
                    let path: Vec<String> = heading_levels[..=idx]
                        .iter()
                        .filter_map(|x| x.clone())
                        .collect();
                    result.push((offset, path));
                }
            }
            _ => {}
        }
    }

    result
}

/// Look up the heading path at a given byte offset using binary search.
///
/// Returns the path from the most recent heading at or before `offset`,
/// or an empty vec if no heading precedes the offset.
pub fn heading_path_at(index: &[(usize, Vec<String>)], offset: usize) -> Vec<String> {
    match index.binary_search_by_key(&offset, |&(o, _)| o) {
        Ok(pos) => index[pos].1.clone(),
        Err(0) => vec![],
        Err(pos) => index[pos - 1].1.clone(),
    }
}

fn heading_level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_markdown_produces_empty_index() {
        let idx = build_heading_index("");
        assert!(idx.is_empty());
    }

    #[test]
    fn prose_without_headings_produces_empty_index() {
        let idx = build_heading_index("Just some prose with no headings.");
        assert!(idx.is_empty());
    }

    #[test]
    fn single_h1_index_entry() {
        let md = "# Introduction\n\nSome paragraph.";
        let idx = build_heading_index(md);
        assert_eq!(idx.len(), 1);
        assert_eq!(idx[0].1, vec!["Introduction"]);
    }

    #[test]
    fn nested_headings_produce_cumulative_paths() {
        let md = "# API\n\n## Auth\n\nContent.\n";
        let idx = build_heading_index(md);
        assert_eq!(idx.len(), 2);
        assert_eq!(idx[0].1, vec!["API"]);
        assert_eq!(idx[1].1, vec!["API", "Auth"]);
    }

    #[test]
    fn heading_path_resets_when_parent_changes() {
        let md = "# A\n\n## B\n\n# C\n\nContent.";
        let idx = build_heading_index(md);
        let last = idx.last().unwrap();
        assert_eq!(last.1, vec!["C"], "new H1 clears all deeper levels");
    }

    #[test]
    fn heading_path_at_before_first_heading_is_empty() {
        let md = "Preamble text.\n\n# Heading\n\nContent.";
        let idx = build_heading_index(md);
        let path = heading_path_at(&idx, 0);
        assert!(path.is_empty(), "before any heading, path must be empty");
    }

    #[test]
    fn heading_path_at_after_heading_returns_path() {
        let md = "# Intro\n\nParagraph.";
        let idx = build_heading_index(md);
        // The heading starts at offset 0; content after it should resolve to ["Intro"].
        let h_offset = idx[0].0;
        // Any offset >= h_offset should resolve to ["Intro"].
        let path = heading_path_at(&idx, h_offset + 10);
        assert_eq!(path, vec!["Intro"]);
    }

    #[test]
    fn binary_search_exact_match() {
        let md = "# First\n\n# Second\n\nContent.";
        let idx = build_heading_index(md);
        // Exact match on offset of "Second"
        let second_offset = idx[1].0;
        let path = heading_path_at(&idx, second_offset);
        assert_eq!(path, vec!["Second"]);
    }

    #[test]
    fn three_level_path() {
        let md = "# A\n\n## B\n\n### C\n\nDeep content.";
        let idx = build_heading_index(md);
        assert_eq!(idx.len(), 3);
        assert_eq!(idx[2].1, vec!["A", "B", "C"]);
        // Path after all three headings
        let path = heading_path_at(&idx, idx[2].0 + 5);
        assert_eq!(path, vec!["A", "B", "C"]);
    }
}
