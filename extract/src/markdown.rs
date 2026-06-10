//! Markdown extraction using pulldown-cmark.
//!
//! Extracts normalized text + Block structures from CommonMark Markdown.
//! Headings become `heading_path` entries. Code fences are preserved as
//! `BlockKind::Code` blocks.

use crate::{make_block, ExtractionOutput};
use localdb_core::{BlockKind, Error, Span};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Extract a Markdown document.
///
/// # Normalization
/// - Heading text is placed in the normalized text verbatim.
/// - Paragraphs, code blocks, list items, blockquotes all appear in order.
/// - A single trailing newline is appended to each block's text contribution.
pub fn extract_markdown(input: &str) -> Result<ExtractionOutput, Error> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    // Note: ENABLE_TABLES is intentionally omitted — table events are not yet
    // handled by the state machine (tables fall through to the idle default and
    // their content is silently dropped).  Disabling the extension keeps the
    // parser in CommonMark mode for tables so they are emitted as paragraphs
    // rather than structured but discarded table events.

    let parser = Parser::new_ext(input, options);

    // Normalized text accumulates here; blocks index into it via byte spans.
    let mut normalized = String::new();

    // Current heading_path reflecting the most recent headings at each level.
    // Index 0 = H1 … Index 5 = H6. We rebuild heading_path on each new heading.
    let mut heading_levels: [Option<String>; 6] = Default::default();

    let mut blocks = Vec::new();
    let mut ordinal: usize = 0;

    // Accumulator state
    enum State {
        Idle,
        InHeading(u8), // current heading level
        InParagraph,
        InCode,
        InList,
        InListItem,
        InBlockquote,
    }

    let mut state = State::Idle;
    let mut text_buf = String::new();
    let mut title: Option<String> = None;

    // We'll collect all events first so we can handle nesting.
    let events: Vec<Event> = parser.collect();

    for event in events {
        match &state {
            State::Idle => match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    let lvl = heading_level_to_u8(level);
                    text_buf.clear();
                    state = State::InHeading(lvl);
                }
                Event::Start(Tag::Paragraph) => {
                    text_buf.clear();
                    state = State::InParagraph;
                }
                Event::Start(Tag::CodeBlock(_)) => {
                    text_buf.clear();
                    state = State::InCode;
                }
                Event::Start(Tag::List(_)) => {
                    text_buf.clear();
                    state = State::InList;
                }
                Event::Start(Tag::BlockQuote(_)) => {
                    text_buf.clear();
                    state = State::InBlockquote;
                }
                _ => {}
            },

            State::InHeading(lvl) => {
                let lvl = *lvl;
                match event {
                    Event::Text(t) => text_buf.push_str(&t),
                    Event::SoftBreak | Event::HardBreak => text_buf.push(' '),
                    Event::End(TagEnd::Heading(_)) => {
                        // Finalize heading
                        let heading_text = std::mem::take(&mut text_buf);
                        let start = normalized.len();
                        normalized.push_str(&heading_text);
                        normalized.push('\n');
                        let end = normalized.len();

                        // Update heading path: truncate deeper levels, set this level
                        let idx = (lvl - 1) as usize;
                        heading_levels[idx] = Some(heading_text.clone());
                        // Clear deeper levels
                        for level_slot in heading_levels.iter_mut().skip(idx + 1) {
                            *level_slot = None;
                        }

                        let heading_path: Vec<String> = heading_levels[..=idx]
                            .iter()
                            .filter_map(|x| x.clone())
                            .collect();

                        // First H1 becomes the document title
                        if lvl == 1 && title.is_none() {
                            title = Some(heading_text.clone());
                        }

                        // Push to heading_stack so paragraph blocks can find their path
                        // (heading_levels already has the right state)

                        let block = make_block(
                            ordinal,
                            BlockKind::Heading,
                            heading_text,
                            Span::new(start, end),
                            heading_path,
                        );
                        blocks.push(block);
                        ordinal += 1;
                        state = State::Idle;
                    }
                    _ => {}
                }
            }

            State::InParagraph => match event {
                Event::Text(t) => text_buf.push_str(&t),
                Event::Code(c) => {
                    text_buf.push('`');
                    text_buf.push_str(&c);
                    text_buf.push('`');
                }
                Event::SoftBreak => text_buf.push(' '),
                Event::HardBreak => {
                    text_buf.push('\n');
                }
                Event::End(TagEnd::Paragraph) => {
                    let para_text = std::mem::take(&mut text_buf);
                    if !para_text.trim().is_empty() {
                        let start = normalized.len();
                        normalized.push_str(&para_text);
                        normalized.push('\n');
                        let end = normalized.len();

                        let heading_path = current_heading_path(&heading_levels);
                        let block = make_block(
                            ordinal,
                            BlockKind::Paragraph,
                            para_text,
                            Span::new(start, end),
                            heading_path,
                        );
                        blocks.push(block);
                        ordinal += 1;
                    }
                    state = State::Idle;
                }
                _ => {}
            },

            State::InCode => match event {
                Event::Text(t) => text_buf.push_str(&t),
                Event::End(TagEnd::CodeBlock) => {
                    let code_text = std::mem::take(&mut text_buf);
                    if !code_text.trim().is_empty() {
                        let start = normalized.len();
                        normalized.push_str(&code_text);
                        // Ensure trailing newline
                        if !normalized.ends_with('\n') {
                            normalized.push('\n');
                        }
                        let end = normalized.len();

                        let heading_path = current_heading_path(&heading_levels);
                        let block = make_block(
                            ordinal,
                            BlockKind::Code,
                            code_text,
                            Span::new(start, end),
                            heading_path,
                        );
                        blocks.push(block);
                        ordinal += 1;
                    }
                    state = State::Idle;
                }
                _ => {}
            },

            State::InList => match event {
                Event::Start(Tag::Item) => {
                    // nested list items — accumulate text
                    state = State::InListItem;
                    text_buf.clear();
                }
                Event::End(TagEnd::List(_)) => {
                    state = State::Idle;
                }
                _ => {}
            },

            State::InListItem => match event {
                Event::Text(t) => text_buf.push_str(&t),
                Event::SoftBreak => text_buf.push(' '),
                Event::Start(Tag::Paragraph) => {}
                Event::End(TagEnd::Paragraph) => {
                    // don't flush yet — wait for item end
                }
                Event::End(TagEnd::Item) => {
                    let item_text = std::mem::take(&mut text_buf);
                    if !item_text.trim().is_empty() {
                        let start = normalized.len();
                        normalized.push_str(&item_text);
                        normalized.push('\n');
                        let end = normalized.len();

                        let heading_path = current_heading_path(&heading_levels);
                        let block = make_block(
                            ordinal,
                            BlockKind::ListItem,
                            item_text,
                            Span::new(start, end),
                            heading_path,
                        );
                        blocks.push(block);
                        ordinal += 1;
                    }
                    // Back to InList state
                    state = State::InList;
                }
                _ => {}
            },

            State::InBlockquote => match event {
                Event::Text(t) => text_buf.push_str(&t),
                Event::SoftBreak | Event::HardBreak => text_buf.push('\n'),
                Event::End(TagEnd::BlockQuote(_)) => {
                    let bq_text = std::mem::take(&mut text_buf);
                    if !bq_text.trim().is_empty() {
                        let start = normalized.len();
                        normalized.push_str(&bq_text);
                        normalized.push('\n');
                        let end = normalized.len();

                        let heading_path = current_heading_path(&heading_levels);
                        let block = make_block(
                            ordinal,
                            BlockKind::Blockquote,
                            bq_text,
                            Span::new(start, end),
                            heading_path,
                        );
                        blocks.push(block);
                        ordinal += 1;
                    }
                    state = State::Idle;
                }
                _ => {}
            },
        }
    }

    Ok(ExtractionOutput {
        text: normalized,
        blocks,
        title,
    })
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

/// Build the current heading path from the heading_levels array.
fn current_heading_path(heading_levels: &[Option<String>; 6]) -> Vec<String> {
    heading_levels.iter().filter_map(|x| x.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::BlockKind;

    #[test]
    fn extracts_title_from_h1() {
        let md = "# My Title\n\nSome paragraph text.\n";
        let out = extract_markdown(md).unwrap();
        assert_eq!(out.title, Some("My Title".to_string()));
    }

    #[test]
    fn heading_block_has_correct_kind() {
        let md = "# Title\n";
        let out = extract_markdown(md).unwrap();
        assert!(
            out.blocks.iter().any(|b| b.kind == BlockKind::Heading),
            "Expected a Heading block"
        );
    }

    #[test]
    fn paragraph_block_has_correct_kind() {
        let md = "Some paragraph text.\n";
        let out = extract_markdown(md).unwrap();
        assert!(
            out.blocks.iter().any(|b| b.kind == BlockKind::Paragraph),
            "Expected a Paragraph block"
        );
    }

    #[test]
    fn code_fence_block_has_correct_kind() {
        let md = "```rust\nlet x = 1;\n```\n";
        let out = extract_markdown(md).unwrap();
        assert!(
            out.blocks.iter().any(|b| b.kind == BlockKind::Code),
            "Expected a Code block"
        );
    }

    #[test]
    fn spans_index_into_normalized_text_exactly() {
        let md = "# Title\n\nParagraph text here.\n";
        let out = extract_markdown(md).unwrap();
        for block in &out.blocks {
            let span_text = &out.text[block.span.start..block.span.end];
            assert!(
                span_text.contains(block.text.trim()),
                "Span [{}, {}) should contain block text {:?}, got {:?}",
                block.span.start,
                block.span.end,
                block.text,
                span_text
            );
        }
    }

    #[test]
    fn nested_headings_produce_correct_heading_paths() {
        let md = "# Level 1\n\n## Level 1.1\n\nContent.\n";
        let out = extract_markdown(md).unwrap();

        // Find H2 block
        let h2_block = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Heading && b.text == "Level 1.1")
            .expect("Expected H2 block");
        assert_eq!(
            h2_block.heading_path,
            vec!["Level 1", "Level 1.1"],
            "H2 should have path [Level 1, Level 1.1]"
        );

        // Paragraph under H2 should have the same heading path
        let para = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph)
            .expect("Expected paragraph block");
        assert_eq!(
            para.heading_path,
            vec!["Level 1", "Level 1.1"],
            "Paragraph should have path [Level 1, Level 1.1]"
        );
    }

    #[test]
    fn heading_path_resets_when_parent_heading_changes() {
        let md = "# H1\n\n## H2a\n\nContent A.\n\n## H2b\n\nContent B.\n";
        let out = extract_markdown(md).unwrap();

        let content_b = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph && b.text.contains("Content B"))
            .expect("Expected Content B paragraph");
        assert_eq!(
            content_b.heading_path,
            vec!["H1", "H2b"],
            "Content B should be under H2b, not H2a"
        );
    }

    #[test]
    fn deep_heading_path_is_correct() {
        let md = "# A\n\n## B\n\n### C\n\nDeep content.\n";
        let out = extract_markdown(md).unwrap();

        let deep = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph && b.text.contains("Deep content"))
            .expect("Expected Deep content block");
        assert_eq!(
            deep.heading_path,
            vec!["A", "B", "C"],
            "Deep content should have path [A, B, C]"
        );
    }

    #[test]
    fn heading_path_cleared_when_going_back_to_h1() {
        let md = "# First\n\n## Sub\n\n# Second\n\nContent.\n";
        let out = extract_markdown(md).unwrap();

        let content = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph && b.text.contains("Content"))
            .expect("Expected Content block");
        assert_eq!(
            content.heading_path,
            vec!["Second"],
            "Content after Second H1 should only have [Second]"
        );
    }

    #[test]
    fn golden_file_simple_md() {
        let md = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/simple.md"
        ))
        .expect("fixture file not found");
        let out = extract_markdown(&md).unwrap();

        // Should have a title
        assert_eq!(out.title, Some("Introduction".to_string()));

        // All spans must be valid within the normalized text
        for block in &out.blocks {
            assert!(
                block.span.end <= out.text.len(),
                "Span end {} exceeds text length {}",
                block.span.end,
                out.text.len()
            );
            assert!(
                block.span.start <= block.span.end,
                "Span start {} > end {}",
                block.span.start,
                block.span.end
            );
        }
    }

    #[test]
    fn golden_file_nested_headings() {
        let md = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/nested_headings.md"
        ))
        .expect("fixture file not found");
        let out = extract_markdown(&md).unwrap();

        // Find the "Level 1.1.1" heading block
        let h3 = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Heading && b.text == "Level 1.1.1")
            .expect("Expected Level 1.1.1 block");
        assert_eq!(h3.heading_path, vec!["Level 1", "Level 1.1", "Level 1.1.1"]);

        // "Deep content" para should be under Level 1 > Level 1.1 > Level 1.1.1
        let deep = out
            .blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph && b.text.contains("Deep content"))
            .expect("Expected Deep content paragraph");
        assert_eq!(
            deep.heading_path,
            vec!["Level 1", "Level 1.1", "Level 1.1.1"]
        );
    }
}
