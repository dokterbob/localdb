//! Convert Markdown text to typed [`Block`]s and derive heading paths.
//!
//! # Usage
//!
//! ```
//! use localdb_core::markdown_blocks::{markdown_to_blocks, heading_path_from_blocks};
//!
//! let blocks = markdown_to_blocks("# Hello\n\nWorld");
//! let path = heading_path_from_blocks(&blocks, 1); // path for block at seq 1
//! assert_eq!(path, vec!["Hello".to_string()]);
//! ```

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::block::{Block, BlockKind};
use crate::ids::content_hash;

// ---------------------------------------------------------------------------
// markdown_to_blocks
// ---------------------------------------------------------------------------

/// Convert a Markdown string to a sequence of typed [`Block`]s.
///
/// YAML front-matter at the very beginning of the document is detected by a
/// pre-scan of the raw string (looking for `---\n` at position 0 and a
/// matching `---\n` closing delimiter) and is yielded as a
/// `BlockKind::Frontmatter { format: "yaml" }` block before any Markdown
/// parsing happens.
///
/// All other content is parsed with `pulldown-cmark` using at minimum the
/// `ENABLE_TABLES` and `ENABLE_STRIKETHROUGH` options. Blocks are assigned
/// sequential `seq` values starting from 0. `location` is always `None`.
pub fn markdown_to_blocks(markdown: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut seq: u32 = 0;

    // -----------------------------------------------------------------------
    // 1. Pre-scan: YAML front-matter detection
    // -----------------------------------------------------------------------
    let (frontmatter_consumed, rest) = extract_frontmatter(markdown);
    if let Some(fm_text) = frontmatter_consumed {
        blocks.push(Block {
            seq,
            kind: BlockKind::Frontmatter {
                format: "yaml".to_string(),
            },
            text: fm_text,
            location: None,
        });
        seq += 1;
    }

    // -----------------------------------------------------------------------
    // 2. Parse the remaining Markdown
    // -----------------------------------------------------------------------
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_DEFINITION_LIST;

    let parser = Parser::new_ext(rest, opts);

    // We accumulate block state using a simple stack-based approach.  The
    // pulldown-cmark stream is a flat sequence of `Start`/`End` events with
    // text events in between; we track nesting depth to decide when a "block"
    // is complete.

    // Active block being assembled.
    struct ActiveBlock {
        kind: ActiveKind,
        /// Accumulated text pieces.
        text: Vec<String>,
        /// Table-specific state.
        table_headers: Vec<String>,
        table_row_count: usize,
        in_table_head: bool,
    }

    enum ActiveKind {
        Heading { level: u8 },
        Paragraph,
        Code { language: Option<String> },
        Quote,
        List { ordered: bool },
        Table,
        Image { src: Option<String> },
    }

    let mut stack: Vec<ActiveBlock> = Vec::new();

    // Helper: push a completed block.
    macro_rules! push_block {
        ($blocks:expr, $seq:expr, $kind:expr, $text:expr) => {{
            let text = $text;
            if !text.is_empty() {
                $blocks.push(Block {
                    seq: $seq,
                    kind: $kind,
                    text,
                    location: None,
                });
                $seq += 1;
            }
        }};
    }

    for event in parser {
        match event {
            // ----------------------------------------------------------------
            // Block start events
            // ----------------------------------------------------------------
            Event::Start(Tag::Heading { level, .. }) => {
                let lv = level as u8;
                stack.push(ActiveBlock {
                    kind: ActiveKind::Heading { level: lv },
                    text: Vec::new(),
                    table_headers: Vec::new(),
                    table_row_count: 0,
                    in_table_head: false,
                });
            }

            Event::Start(Tag::Paragraph) => {
                stack.push(ActiveBlock {
                    kind: ActiveKind::Paragraph,
                    text: Vec::new(),
                    table_headers: Vec::new(),
                    table_row_count: 0,
                    in_table_head: false,
                });
            }

            Event::Start(Tag::CodeBlock(fence)) => {
                let lang: Option<String> = match fence {
                    pulldown_cmark::CodeBlockKind::Fenced(s) => {
                        let s = s.trim().to_string();
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    }
                    pulldown_cmark::CodeBlockKind::Indented => None,
                };
                stack.push(ActiveBlock {
                    kind: ActiveKind::Code { language: lang },
                    text: Vec::new(),
                    table_headers: Vec::new(),
                    table_row_count: 0,
                    in_table_head: false,
                });
            }

            Event::Start(Tag::BlockQuote(_)) => {
                stack.push(ActiveBlock {
                    kind: ActiveKind::Quote,
                    text: Vec::new(),
                    table_headers: Vec::new(),
                    table_row_count: 0,
                    in_table_head: false,
                });
            }

            Event::Start(Tag::List(start)) => {
                stack.push(ActiveBlock {
                    kind: ActiveKind::List {
                        ordered: start.is_some(),
                    },
                    text: Vec::new(),
                    table_headers: Vec::new(),
                    table_row_count: 0,
                    in_table_head: false,
                });
            }

            Event::Start(Tag::Table(_alignments)) => {
                stack.push(ActiveBlock {
                    kind: ActiveKind::Table,
                    text: Vec::new(),
                    table_headers: Vec::new(),
                    table_row_count: 0,
                    in_table_head: false,
                });
            }

            Event::Start(Tag::TableHead) => {
                if let Some(b) = stack.last_mut() {
                    b.in_table_head = true;
                }
            }

            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                let src = Some(dest_url.to_string());
                // alt text comes as Text events inside the Image tag; title is the
                // image title attribute (not the alt). We store src from dest_url.
                // The title attribute is stored separately if present.
                let _ = title; // may use in future
                stack.push(ActiveBlock {
                    kind: ActiveKind::Image { src },
                    text: Vec::new(),
                    table_headers: Vec::new(),
                    table_row_count: 0,
                    in_table_head: false,
                });
            }

            // ----------------------------------------------------------------
            // Block end events
            // ----------------------------------------------------------------
            Event::End(TagEnd::Heading(_)) => {
                if let Some(b) = stack.pop() {
                    let text = b.text.join("");
                    let level = match b.kind {
                        ActiveKind::Heading { level } => level,
                        _ => 1,
                    };
                    push_block!(blocks, seq, BlockKind::Heading { level }, text);
                }
            }

            Event::End(TagEnd::Paragraph) => {
                if let Some(b) = stack.pop() {
                    let text = b.text.join("");
                    match b.kind {
                        ActiveKind::Paragraph => {
                            // If the parent on the stack is a container (Quote, List),
                            // propagate the text up rather than emitting a new block.
                            if let Some(parent) = stack.last_mut() {
                                match parent.kind {
                                    ActiveKind::Quote | ActiveKind::List { .. } => {
                                        if !parent.text.is_empty() {
                                            parent.text.push(" ".to_string());
                                        }
                                        parent.text.push(text);
                                        // Continue — do NOT emit a standalone Paragraph.
                                        continue;
                                    }
                                    _ => {}
                                }
                            }
                            // Top-level paragraph: emit it.
                            push_block!(blocks, seq, BlockKind::Paragraph, text);
                        }
                        _ => {
                            // Restore — shouldn't happen normally.
                            stack.push(ActiveBlock {
                                kind: b.kind,
                                text: b.text,
                                table_headers: b.table_headers,
                                table_row_count: b.table_row_count,
                                in_table_head: b.in_table_head,
                            });
                        }
                    }
                }
            }

            Event::End(TagEnd::CodeBlock) => {
                if let Some(b) = stack.pop() {
                    let text = b.text.join("");
                    let language = match b.kind {
                        ActiveKind::Code { language } => language,
                        _ => None,
                    };
                    push_block!(blocks, seq, BlockKind::Code { language }, text);
                }
            }

            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(b) = stack.pop() {
                    let text = b.text.join(" ");
                    push_block!(blocks, seq, BlockKind::Quote, text);
                }
            }

            Event::End(TagEnd::List(_)) => {
                if let Some(b) = stack.pop() {
                    let ordered = match b.kind {
                        ActiveKind::List { ordered } => ordered,
                        _ => false,
                    };
                    let text = b.text.join("\n");
                    push_block!(blocks, seq, BlockKind::List { ordered }, text);
                }
            }

            Event::End(TagEnd::Table) => {
                if let Some(b) = stack.pop() {
                    let headers = b.table_headers.clone();
                    let rows = b.table_row_count;
                    let text = b.text.join(" ");
                    push_block!(blocks, seq, BlockKind::Table { headers, rows }, text);
                }
            }

            Event::End(TagEnd::TableHead) => {
                if let Some(b) = stack.last_mut() {
                    b.in_table_head = false;
                }
            }

            Event::End(TagEnd::TableRow) => {
                if let Some(b) = stack.last_mut() {
                    if !b.in_table_head {
                        b.table_row_count += 1;
                    }
                }
            }

            Event::End(TagEnd::Image) => {
                if let Some(b) = stack.pop() {
                    let src = match &b.kind {
                        ActiveKind::Image { src } => src.clone(),
                        _ => None,
                    };
                    // Text events inside an image contain the alt text.
                    let alt_text = b.text.join("");
                    let alt = if alt_text.is_empty() {
                        None
                    } else {
                        Some(alt_text.clone())
                    };
                    let text = alt_text;
                    // Always emit Image block (even empty text)
                    blocks.push(Block {
                        seq,
                        kind: BlockKind::Image { alt, src },
                        text,
                        location: None,
                    });
                    seq += 1;
                }
            }

            // ----------------------------------------------------------------
            // Text events — accumulate into current block
            // ----------------------------------------------------------------
            Event::Text(t) => {
                if let Some(b) = stack.last_mut() {
                    match &b.kind {
                        ActiveKind::Table => {
                            if b.in_table_head {
                                b.table_headers.push(t.to_string());
                            } else {
                                b.text.push(t.to_string());
                            }
                        }
                        _ => b.text.push(t.to_string()),
                    }
                }
            }

            Event::Code(t) => {
                // Inline code inside a paragraph/heading.
                if let Some(b) = stack.last_mut() {
                    b.text.push(t.to_string());
                }
            }

            Event::SoftBreak | Event::HardBreak => {
                if let Some(b) = stack.last_mut() {
                    b.text.push(" ".to_string());
                }
            }

            // ----------------------------------------------------------------
            // Item delimiters inside lists
            // ----------------------------------------------------------------
            Event::Start(Tag::Item) => {
                // Add separator before each item except the first.
                if let Some(b) = stack.last_mut() {
                    if matches!(b.kind, ActiveKind::List { .. }) && !b.text.is_empty() {
                        b.text.push("\n".to_string());
                    }
                }
            }

            Event::Html(t) => {
                // Flush any open block before emitting an HTML block.
                // HTML blocks are stand-alone: they do not nest inside a
                // paragraph or other container.
                while let Some(b) = stack.pop() {
                    let text = b.text.join(" ");
                    push_block!(blocks, seq, BlockKind::Paragraph, text);
                }
                let trimmed = t.trim().to_string();
                if !trimmed.is_empty() {
                    blocks.push(Block {
                        seq,
                        kind: BlockKind::Paragraph,
                        text: trimmed,
                        location: None,
                    });
                    seq += 1;
                }
            }

            // Ignore everything else: HR, footnotes, soft/hard breaks, and
            // inline HTML fragments (Event::InlineHtml, e.g. <br>, <em>).
            // Inline HTML appears inside paragraphs/headings and is silently
            // dropped; the surrounding text is still captured by Event::Text.
            _ => {}
        }
    }

    blocks
}

// ---------------------------------------------------------------------------
// compute_blocks_hash
// ---------------------------------------------------------------------------

/// Compute a content hash from the concatenation of all block texts.
///
/// Block texts are concatenated directly with no separator, matching the
/// spec's "ordered block canonical texts concatenated" definition.
pub fn compute_blocks_hash(blocks: &[Block]) -> String {
    let combined: String = blocks.iter().map(|b| b.text.as_str()).collect();
    content_hash(&combined)
}

// ---------------------------------------------------------------------------
// extract_frontmatter
// ---------------------------------------------------------------------------

/// Detect YAML front-matter at the very beginning of `markdown`.
///
/// Returns `(Some(body), rest)` where `body` is the text between the `---`
/// delimiters and `rest` is everything after the closing `---\n` (or `---`
/// at end-of-file).  Returns `(None, markdown)` if no front-matter is
/// present.
fn extract_frontmatter(markdown: &str) -> (Option<String>, &str) {
    // Determine the opening delimiter length (LF or CRLF).
    let open_len = if markdown.starts_with("---\r\n") {
        5
    } else if markdown.starts_with("---\n") {
        4
    } else {
        return (None, markdown);
    };
    let after_open = &markdown[open_len..];

    // Try the closing delimiter — CRLF first, then LF.
    let (close_pos, close_len) = if let Some(pos) = after_open.find("\n---\r\n") {
        (pos, 6) // "\n---\r\n".len()
    } else if let Some(pos) = after_open.find("\n---\n") {
        (pos, 5) // "\n---\n".len()
    } else {
        // Check for "---" at end of file (no trailing newline).
        if let Some(pos) = after_open.find("\n---") {
            let candidate = &after_open[pos + 1..];
            if candidate == "---" || candidate == "---\r" {
                let body = &after_open[..pos];
                return (Some(body.to_string()), "");
            }
        }
        return (None, markdown);
    };

    let body = &after_open[..close_pos];
    let rest = &markdown[open_len + close_pos + close_len..];
    (Some(body.to_string()), rest)
}

// ---------------------------------------------------------------------------
// heading_path_from_blocks
// ---------------------------------------------------------------------------

/// Derive the heading path active at block `target_seq`.
///
/// Collects all heading blocks with `seq < target_seq` and builds the
/// accumulated heading path the same way `heading_index.rs` does: headings
/// replace all path entries at the same or deeper level.
pub fn heading_path_from_blocks(blocks: &[Block], target_seq: u32) -> Vec<String> {
    // path[i] holds the most recent heading at level (i+1)
    let mut path: Vec<Option<String>> = vec![None; 6]; // levels 1-6

    for block in blocks {
        if block.seq >= target_seq {
            break;
        }
        if let BlockKind::Heading { level } = &block.kind {
            let lv = (*level as usize).clamp(1, 6);
            let idx = lv - 1;
            path[idx] = Some(block.text.clone());
            // Clear deeper levels.
            for deeper in &mut path[lv..] {
                *deeper = None;
            }
        }
    }

    // Build the result: take entries up to the last non-None slot.
    let last_some = path.iter().rposition(|e| e.is_some());
    match last_some {
        None => vec![],
        Some(last) => path[..=last]
            .iter()
            .map(|e| e.clone().unwrap_or_default())
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a well-structured Markdown doc and verify block count and kinds.
    #[test]
    fn markdown_to_blocks_basic() {
        let md = "\
# Introduction

This is the first paragraph.

## Setup

Install with `cargo install localdb`.

```rust
fn main() {}
```

- Item one
- Item two

> A blockquote here.
";

        let blocks = markdown_to_blocks(md);
        assert!(
            blocks.len() >= 5,
            "expected at least 5 blocks, got {}: {:?}",
            blocks.len(),
            blocks.iter().map(|b| b.kind.kind_str()).collect::<Vec<_>>()
        );

        // Seqs are 0-indexed and sequential.
        for (i, b) in blocks.iter().enumerate() {
            assert_eq!(b.seq, i as u32, "seq mismatch at position {}", i);
        }

        // Find a heading block with level 1.
        let h1 = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::Heading { level: 1 }));
        assert!(h1.is_some(), "expected an h1 heading");
        assert_eq!(h1.unwrap().text, "Introduction");

        // Find an h2 block.
        let h2 = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::Heading { level: 2 }));
        assert!(h2.is_some(), "expected an h2 heading");
        assert_eq!(h2.unwrap().text, "Setup");

        // Find a code block.
        let code = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::Code { .. }));
        assert!(code.is_some(), "expected a code block");
        let code_b = code.unwrap();
        if let BlockKind::Code { language } = &code_b.kind {
            assert_eq!(language.as_deref(), Some("rust"));
        }

        // Find a list block.
        let list = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::List { .. }));
        assert!(list.is_some(), "expected a list block");
        assert!(list.unwrap().text.contains("Item one"));

        // Find a quote block.
        let quote = blocks.iter().find(|b| matches!(&b.kind, BlockKind::Quote));
        assert!(quote.is_some(), "expected a blockquote");
    }

    /// Verify heading_path_from_blocks output.
    #[test]
    fn heading_path_from_blocks_basic() {
        let md = "# Top\n\n## Sub\n\nSome text.\n\n### Deep\n\nDeep text.\n";
        let blocks = markdown_to_blocks(md);

        // Block at seq 0 is the h1 "Top"
        assert_eq!(heading_path_from_blocks(&blocks, 0), Vec::<String>::new());

        // Paragraph after "# Top" — seq 1 (h1), seq 2 (paragraph)
        // path should be ["Top"]
        let para_seq = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph)
            .map(|b| b.seq)
            .unwrap();
        let path = heading_path_from_blocks(&blocks, para_seq);
        assert!(
            path.contains(&"Top".to_string()),
            "path should contain Top; got {:?}",
            path
        );

        // At the deep text block, path should include all three headings.
        let deep_text_seq = blocks
            .iter()
            .rev()
            .find(|b| b.kind == BlockKind::Paragraph)
            .map(|b| b.seq)
            .unwrap();
        let deep_path = heading_path_from_blocks(&blocks, deep_text_seq);
        assert!(deep_path.contains(&"Top".to_string()));
        assert!(deep_path.contains(&"Sub".to_string()));
        assert!(deep_path.contains(&"Deep".to_string()));
    }

    /// Heading path resets sub-headings when a new higher-level heading appears.
    #[test]
    fn heading_path_resets_on_new_parent() {
        let md = "# A\n\n## A1\n\n# B\n\nContent.\n";
        let blocks = markdown_to_blocks(md);
        // After "# B", a paragraph's path should be ["B"], not ["A", "A1"] or ["B", "A1"].
        let content_seq = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Paragraph)
            .map(|b| b.seq)
            .unwrap();
        let _path = heading_path_from_blocks(&blocks, content_seq);
        // Content is after "# B" so path = ["B"]
        // Actually content might come before "# B" — verify.
        // All paragraphs:
        let paras: Vec<_> = blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Paragraph)
            .collect();
        let last_para = paras.last().unwrap();
        let path = heading_path_from_blocks(&blocks, last_para.seq);
        assert_eq!(
            path,
            vec!["B".to_string()],
            "after # B, path must be just [B]; got {:?}",
            path
        );
    }

    /// YAML front-matter is extracted as a Frontmatter block.
    #[test]
    fn frontmatter_detected() {
        let md = "---\ntitle: Hello\nauthor: Bob\n---\n\n# Content\n\nText here.\n";
        let blocks = markdown_to_blocks(md);
        assert!(!blocks.is_empty());
        let fm = &blocks[0];
        assert!(
            matches!(&fm.kind, BlockKind::Frontmatter { format } if format == "yaml"),
            "first block should be frontmatter; got {:?}",
            fm.kind
        );
        assert!(fm.text.contains("title: Hello"));
        // Heading comes after frontmatter.
        let heading = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::Heading { .. }));
        assert!(
            heading.is_some(),
            "heading should be parsed after frontmatter"
        );
    }

    /// No front-matter when the document doesn't start with `---\n`.
    #[test]
    fn no_frontmatter_when_absent() {
        let md = "# Just a heading\n\nSome text.\n";
        let blocks = markdown_to_blocks(md);
        assert!(!blocks.is_empty());
        assert!(
            !matches!(&blocks[0].kind, BlockKind::Frontmatter { .. }),
            "should not detect frontmatter when absent"
        );
    }

    /// Table blocks carry headers and row count.
    #[test]
    fn table_block() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let blocks = markdown_to_blocks(md);
        let table = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::Table { .. }));
        assert!(table.is_some(), "expected a table block");
        let table_b = table.unwrap();
        if let BlockKind::Table { headers, rows } = &table_b.kind {
            assert_eq!(headers, &vec!["A".to_string(), "B".to_string()]);
            assert_eq!(*rows, 2, "expected 2 data rows");
        }
    }

    /// Empty markdown produces no blocks.
    #[test]
    fn empty_markdown_produces_no_blocks() {
        let blocks = markdown_to_blocks("");
        assert!(blocks.is_empty());
    }

    /// Heading levels 1-6 are all recognized.
    #[test]
    fn all_heading_levels() {
        let md = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\n";
        let blocks = markdown_to_blocks(md);
        let heading_levels: Vec<u8> = blocks
            .iter()
            .filter_map(|b| {
                if let BlockKind::Heading { level } = &b.kind {
                    Some(*level)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(heading_levels, vec![1, 2, 3, 4, 5, 6]);
    }

    /// heading_path_from_blocks with no headings returns empty.
    #[test]
    fn heading_path_no_headings() {
        let md = "Just plain text, no headings.";
        let blocks = markdown_to_blocks(md);
        let path = heading_path_from_blocks(&blocks, 10);
        assert!(path.is_empty());
    }

    /// HTML blocks must not be silently dropped.
    #[test]
    fn html_block_not_silently_dropped() {
        let md = "# Before\n\n<div>raw HTML content</div>\n\nAfter paragraph.\n";
        let blocks = markdown_to_blocks(md);
        let has_html = blocks.iter().any(|b| b.text.contains("raw HTML content"));
        assert!(has_html, "HTML block must not be silently dropped");
        assert!(blocks.len() >= 3);
    }

    /// Frontmatter with CRLF line endings is detected correctly.
    #[test]
    fn frontmatter_detected_with_crlf() {
        let md = "---\r\ntitle: Hello\r\n---\r\n\r\n# Content\r\n";
        let blocks = markdown_to_blocks(md);
        assert!(!blocks.is_empty());
        assert!(
            matches!(&blocks[0].kind, BlockKind::Frontmatter { format } if format == "yaml"),
            "first block should be CRLF frontmatter; got {:?}",
            blocks[0].kind
        );
        assert!(blocks[0].text.contains("title: Hello"));
    }
}
