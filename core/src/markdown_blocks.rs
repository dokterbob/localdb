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

use crate::block::{Block, BlockKind, BlockLocation};
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
    markdown_to_blocks_with_pages(markdown, &[])
}

/// Like [`markdown_to_blocks`], but stamps each block's `location.page` by
/// resolving the block's first contributing source byte against `page_starts`
/// — `(byte_offset, 1-based page number)` pairs, ascending in both fields, as
/// produced by PDF extraction (`ParsedDocument::page_starts`).
///
/// **Page attribution rule** (specs/02-domain-model.md §6): a block's page is
/// the page containing its *first contributing byte*. Blocks are never split
/// at page boundaries — a coarse `Text` run crossing a page break carries the
/// page it starts on.
///
/// With an empty `page_starts` this is behavior-identical to
/// [`markdown_to_blocks`]: every block's `location` stays `None`.
pub fn markdown_to_blocks_with_pages(markdown: &str, page_starts: &[(usize, u32)]) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut seq: u32 = 0;

    // Resolve a byte offset (into `markdown`) to a location carrying the page
    // that starts at or before it. Offsets before the first entry (possible
    // when page 1 contributed no content) attribute to the first listed page.
    let page_at = |offset: usize| -> Option<BlockLocation> {
        if page_starts.is_empty() {
            return None;
        }
        let idx = page_starts.partition_point(|&(start, _)| start <= offset);
        let page = page_starts[idx.saturating_sub(1)].1;
        Some(BlockLocation {
            page: Some(page),
            ..Default::default()
        })
    };

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
            location: page_at(0),
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

    // `rest` is always a suffix of `markdown` (possibly all of it, possibly
    // empty), so event ranges — relative to `rest` — map into `markdown`
    // coordinates by adding the suffix's start offset.
    let rest_base = markdown.len() - rest.len();

    let parser = Parser::new_ext(rest, opts).into_offset_iter();

    // We accumulate block state using a simple stack-based approach.  The
    // pulldown-cmark stream is a flat sequence of `Start`/`End` events with
    // text events in between; we track nesting depth to decide when a "block"
    // is complete.

    // Active block being assembled.
    struct ActiveBlock {
        kind: ActiveKind,
        /// Byte offset (into `markdown`) of the element's `Start` event —
        /// the block's first contributing byte, for page attribution.
        start: usize,
        /// Accumulated text pieces.
        text: Vec<String>,
        /// Table-specific state.
        table_headers: Vec<String>,
        table_rows: Vec<Vec<String>>,
        table_row: Vec<String>,
        table_cell: Vec<String>,
        table_row_count: usize,
        in_table_head: bool,
    }

    impl ActiveBlock {
        fn new(kind: ActiveKind, start: usize) -> Self {
            ActiveBlock {
                kind,
                start,
                text: Vec::new(),
                table_headers: Vec::new(),
                table_rows: Vec::new(),
                table_row: Vec::new(),
                table_cell: Vec::new(),
                table_row_count: 0,
                in_table_head: false,
            }
        }
    }

    enum ActiveKind {
        Heading { level: u8 },
        Paragraph,
        Code { language: Option<String> },
        Quote,
        List,
        Table,
        Image { src: Option<String> },
    }

    let mut stack: Vec<ActiveBlock> = Vec::new();

    // Running-text accumulator: holds the finished text of each consecutive
    // running-text element (paragraph, list, blockquote, HTML block). Flushed
    // into a single `BlockKind::Text` block at structural boundaries (heading,
    // code, table, image) and once more after the event loop.
    let mut text_accum: Vec<String> = Vec::new();
    // Earliest source offset among the accumulator's contributors — the
    // eventual `Text` block's first contributing byte.
    let mut accum_start: Option<usize> = None;

    // Helper: push a completed block at the given source offset.
    macro_rules! push_block {
        ($blocks:expr, $seq:expr, $kind:expr, $text:expr, $start:expr) => {{
            let text = $text;
            if !text.is_empty() {
                $blocks.push(Block {
                    seq: $seq,
                    kind: $kind,
                    text,
                    location: page_at($start),
                });
                $seq += 1;
            }
        }};
    }

    // Helper: append finished element text to the running-text accumulator,
    // tracking the earliest contributing offset.
    macro_rules! accum_push {
        ($text:expr, $start:expr) => {{
            let text = $text;
            if !text.is_empty() {
                let start: usize = $start;
                accum_start = Some(accum_start.map_or(start, |s| s.min(start)));
                text_accum.push(text);
            }
        }};
    }

    // Helper: flush the running-text accumulator into a single `Text` block.
    macro_rules! flush_text {
        ($blocks:expr, $seq:expr, $accum:expr) => {{
            if !$accum.is_empty() {
                let text = $accum.join("\n\n");
                let location = accum_start.and_then(page_at);
                $blocks.push(Block {
                    seq: $seq,
                    kind: BlockKind::Text,
                    text,
                    location,
                });
                $seq += 1;
                $accum.clear();
            }
            accum_start = None;
        }};
    }

    for (event, range) in parser {
        // The element's first byte in `markdown` coordinates. For `Start`
        // events the range covers the whole element, so `range.start` is the
        // element's first contributing byte.
        let offset = rest_base + range.start;
        match event {
            // ----------------------------------------------------------------
            // Block start events
            // ----------------------------------------------------------------
            Event::Start(Tag::Heading { level, .. }) => {
                flush_text!(blocks, seq, text_accum);
                let lv = level as u8;
                stack.push(ActiveBlock::new(ActiveKind::Heading { level: lv }, offset));
            }

            Event::Start(Tag::Paragraph) => {
                stack.push(ActiveBlock::new(ActiveKind::Paragraph, offset));
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
                flush_text!(blocks, seq, text_accum);
                stack.push(ActiveBlock::new(
                    ActiveKind::Code { language: lang },
                    offset,
                ));
            }

            Event::Start(Tag::BlockQuote(_)) => {
                stack.push(ActiveBlock::new(ActiveKind::Quote, offset));
            }

            Event::Start(Tag::List(_start)) => {
                stack.push(ActiveBlock::new(ActiveKind::List, offset));
            }

            Event::Start(Tag::Table(_alignments)) => {
                flush_text!(blocks, seq, text_accum);
                stack.push(ActiveBlock::new(ActiveKind::Table, offset));
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
                flush_text!(blocks, seq, text_accum);
                stack.push(ActiveBlock::new(ActiveKind::Image { src }, offset));
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
                    push_block!(blocks, seq, BlockKind::Heading { level }, text, b.start);
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
                                    ActiveKind::Quote | ActiveKind::List => {
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
                            // Top-level paragraph: feed the running-text accumulator
                            // rather than emitting a standalone block.
                            accum_push!(text, b.start);
                        }
                        _ => {
                            // Restore — shouldn't happen normally.
                            stack.push(b);
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
                    push_block!(blocks, seq, BlockKind::Code { language }, text, b.start);
                }
            }

            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(b) = stack.pop() {
                    let text = b.text.join(" ");
                    accum_push!(text, b.start);
                }
            }

            Event::End(TagEnd::List(_)) => {
                if let Some(b) = stack.pop() {
                    let text = b.text.join("\n");
                    accum_push!(text, b.start);
                }
            }

            Event::End(TagEnd::Table) => {
                if let Some(b) = stack.pop() {
                    let headers = b.table_headers.clone();
                    let rows = b.table_row_count;
                    // Reconstruct pipe-syntax Markdown so downstream consumers
                    // (chunk_table) can re-split the table row-by-row. Cell text
                    // containing `|` is re-escaped to keep each line parseable.
                    let esc = |s: &String| s.replace('|', "\\|");
                    let ncols = b
                        .table_headers
                        .len()
                        .max(b.table_rows.iter().map(Vec::len).max().unwrap_or(0));
                    let text = if ncols == 0 {
                        String::new()
                    } else {
                        let render_row = |cells: &[String]| {
                            let padded: Vec<String> = (0..ncols)
                                .map(|i| cells.get(i).map(esc).unwrap_or_default())
                                .collect();
                            format!("| {} |", padded.join(" | "))
                        };
                        let mut lines = vec![
                            render_row(&b.table_headers),
                            format!("|{}|", vec![" --- "; ncols].join("|")),
                        ];
                        lines.extend(b.table_rows.iter().map(|r| render_row(r)));
                        lines.join("\n")
                    };
                    push_block!(
                        blocks,
                        seq,
                        BlockKind::Table { headers, rows },
                        text,
                        b.start
                    );
                }
            }

            Event::End(TagEnd::TableCell) => {
                if let Some(b) = stack.last_mut() {
                    let cell = b.table_cell.join("").trim().to_string();
                    b.table_cell.clear();
                    if b.in_table_head {
                        b.table_headers.push(cell);
                    } else {
                        b.table_row.push(cell);
                    }
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
                        b.table_rows.push(std::mem::take(&mut b.table_row));
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
                        location: page_at(b.start),
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
                        // Table text accumulates per cell; TagEnd::TableCell
                        // routes the finished cell to headers or the open row.
                        ActiveKind::Table => b.table_cell.push(t.to_string()),
                        _ => b.text.push(t.to_string()),
                    }
                }
            }

            Event::Code(t) => {
                // Inline code inside a paragraph/heading/table cell.
                if let Some(b) = stack.last_mut() {
                    match &b.kind {
                        ActiveKind::Table => b.table_cell.push(t.to_string()),
                        _ => b.text.push(t.to_string()),
                    }
                }
            }

            Event::SoftBreak | Event::HardBreak => {
                if let Some(b) = stack.last_mut() {
                    match &b.kind {
                        ActiveKind::Table => b.table_cell.push(" ".to_string()),
                        _ => b.text.push(" ".to_string()),
                    }
                }
            }

            // ----------------------------------------------------------------
            // Item delimiters inside lists
            // ----------------------------------------------------------------
            Event::Start(Tag::Item) => {
                // Add separator before each item except the first.
                if let Some(b) = stack.last_mut() {
                    if matches!(b.kind, ActiveKind::List) && !b.text.is_empty() {
                        b.text.push("\n".to_string());
                    }
                }
            }

            Event::Html(t) => {
                // HTML blocks are running text: drain any open stack blocks'
                // text into the accumulator, then append this HTML fragment.
                // Do NOT flush — HTML merges with surrounding paragraphs into
                // the same `Text` block.
                while let Some(b) = stack.pop() {
                    let text = b.text.join(" ");
                    accum_push!(text, b.start);
                }
                let trimmed = t.trim().to_string();
                accum_push!(trimmed, offset);
            }

            // Ignore everything else: HR, footnotes, soft/hard breaks, and
            // inline HTML fragments (Event::InlineHtml, e.g. <br>, <em>).
            // Inline HTML appears inside paragraphs/headings and is silently
            // dropped; the surrounding text is still captured by Event::Text.
            _ => {}
        }
    }

    // Flush any trailing running text after the event loop ends.
    flush_text!(blocks, seq, text_accum);
    // Final flush_text! increments seq and resets accum_start; neither is
    // read again after the loop.
    let _ = (seq, accum_start);

    blocks
}

// ---------------------------------------------------------------------------
// compute_blocks_hash
// ---------------------------------------------------------------------------

/// Compute a content hash from block kind, text, and page, joined with separators.
///
/// Each block contributes `"kind:text"` and entries are separated by `\x00`
/// (NUL byte) to prevent cross-block collisions. Including the block kind
/// ensures that structural changes (e.g. paragraph→heading with same text)
/// trigger re-indexing.
///
/// When a block carries a page number (paginated formats — PDF, #103) it is
/// folded in as a `\x01p{page}` suffix, so a **repagination that leaves the
/// text and kinds unchanged still changes the hash** and re-indexes — otherwise
/// the skip-check (which keys on this hash) would leave stored citations on
/// their old, now-wrong pages. Blocks without a page (every non-paginated
/// format) contribute no suffix, so their hashes are unchanged from before this
/// addition — no spurious global reindex.
pub fn compute_blocks_hash(blocks: &[Block]) -> String {
    let combined: String = blocks
        .iter()
        .map(|b| {
            let base = format!("{}:{}", b.kind.kind_str(), b.text);
            match b.location.as_ref().and_then(|loc| loc.page) {
                Some(page) => format!("{base}\x01p{page}"),
                None => base,
            }
        })
        .collect::<Vec<_>>()
        .join("\x00");
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
///
/// `pub` so `extract::markdown` can do a second, independent read of the
/// front-matter body to scan for a `date:` key (issue #251) — this function
/// only carves the block out; it does not change what block-carving does.
pub fn extract_frontmatter(markdown: &str) -> (Option<String>, &str) {
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
        assert_eq!(
            blocks.len(),
            6,
            "expected 6 blocks (h1, text, h2, text, code, text), got {}: {:?}",
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

        // Find a code block — Code still breaks the run into its own block.
        let code = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::Code { .. }));
        assert!(code.is_some(), "expected a code block");
        let code_b = code.unwrap();
        if let BlockKind::Code { language } = &code_b.kind {
            assert_eq!(language.as_deref(), Some("rust"));
        }

        // The list and blockquote fold into the same Text block: no
        // heading/code/table sits between them.
        let list_text = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Text && b.text.contains("Item one"));
        assert!(
            list_text.is_some(),
            "expected a Text block containing the list"
        );
        assert!(
            list_text.unwrap().text.contains("A blockquote here."),
            "list and blockquote should fold into the same Text block: {:?}",
            list_text.unwrap().text
        );

        // Headings and Code still break runs: exactly 3 Text blocks total
        // (para before h2, para before code, list+quote at the end).
        let text_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Text)
            .collect();
        assert_eq!(
            text_blocks.len(),
            3,
            "expected 3 Text blocks: {:?}",
            text_blocks.iter().map(|b| &b.text).collect::<Vec<_>>()
        );
    }

    /// Verify heading_path_from_blocks output.
    #[test]
    fn heading_path_from_blocks_basic() {
        let md = "# Top\n\n## Sub\n\nSome text.\n\n### Deep\n\nDeep text.\n";
        let blocks = markdown_to_blocks(md);

        // Block at seq 0 is the h1 "Top"
        assert_eq!(heading_path_from_blocks(&blocks, 0), Vec::<String>::new());

        // Paragraph after "# Top" — seq 1 (h1), seq 2 (text block)
        // path should be ["Top"]
        let para_seq = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Text)
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
            .find(|b| b.kind == BlockKind::Text)
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
        // After "# B", a text block's path should be ["B"], not ["A", "A1"] or ["B", "A1"].
        let content_seq = blocks
            .iter()
            .find(|b| b.kind == BlockKind::Text)
            .map(|b| b.seq)
            .unwrap();
        let _path = heading_path_from_blocks(&blocks, content_seq);
        // Content is after "# B" so path = ["B"]
        // Actually content might come before "# B" — verify.
        // All text blocks:
        let paras: Vec<_> = blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Text)
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

    /// Consecutive paragraphs, a list, and a blockquote with no intervening
    /// heading/code/table fold into exactly ONE `Text` block.
    #[test]
    fn consecutive_running_text_folds_into_one_text_block() {
        let md = "\
Para one.

Para two.

Para three.

- List item

> Quote line
";
        let blocks = markdown_to_blocks(md);
        assert_eq!(
            blocks.len(),
            1,
            "expected a single Text block, got {}: {:?}",
            blocks.len(),
            blocks.iter().map(|b| b.kind.kind_str()).collect::<Vec<_>>()
        );
        assert_eq!(blocks[0].kind, BlockKind::Text);
        let text = &blocks[0].text;
        assert!(text.contains("Para one."), "missing para one: {text}");
        assert!(text.contains("Para two."), "missing para two: {text}");
        assert!(text.contains("Para three."), "missing para three: {text}");
        assert!(text.contains("List item"), "missing list item: {text}");
        assert!(text.contains("Quote line"), "missing quote line: {text}");
        assert!(
            text.contains("\n\n"),
            "expected \\n\\n separators between folded elements: {text}"
        );
    }

    /// A heading between two paragraphs breaks the run: each paragraph lands
    /// in its own `Text` block, with the heading block in between.
    #[test]
    fn heading_breaks_text_run_into_separate_blocks() {
        let md = "# H\n\npara1\n\n## H2\n\npara2\n";
        let blocks = markdown_to_blocks(md);
        assert_eq!(
            blocks.len(),
            4,
            "expected h1, text(para1), h2, text(para2); got {}: {:?}",
            blocks.len(),
            blocks.iter().map(|b| b.kind.kind_str()).collect::<Vec<_>>()
        );
        assert!(matches!(blocks[0].kind, BlockKind::Heading { level: 1 }));
        assert_eq!(blocks[1].kind, BlockKind::Text);
        assert_eq!(blocks[1].text, "para1");
        assert!(matches!(blocks[2].kind, BlockKind::Heading { level: 2 }));
        assert_eq!(blocks[3].kind, BlockKind::Text);
        assert_eq!(blocks[3].text, "para2");

        // The two Text blocks are distinct — the heading sits between them.
        assert_ne!(blocks[1].text, blocks[3].text);
        assert!(blocks[0].seq < blocks[1].seq);
        assert!(blocks[1].seq < blocks[2].seq);
        assert!(blocks[2].seq < blocks[3].seq);
    }

    /// A fenced code block between two paragraphs breaks the run: Text, Code, Text.
    #[test]
    fn code_block_breaks_text_run_into_separate_blocks() {
        let md = "para1\n\n```\ncode\n```\n\npara2\n";
        let blocks = markdown_to_blocks(md);
        let kinds: Vec<&str> = blocks.iter().map(|b| b.kind.kind_str()).collect();
        assert_eq!(
            kinds,
            vec!["text", "code", "text"],
            "expected Text, Code, Text; got {:?}",
            kinds
        );
        assert_eq!(blocks[0].text, "para1");
        assert_eq!(blocks[2].text, "para2");
    }

    /// heading_path resolves correctly for text blocks after multiple
    /// headings, even though each intervening run is now a single coarse
    /// `Text` block rather than one block per paragraph.
    #[test]
    fn heading_path_resolves_across_coarse_text_blocks() {
        let md = "# A\n\nIntro under A.\n\n## B\n\nText under B.\n\n### C\n\nText under C.\n";
        let blocks = markdown_to_blocks(md);

        let text_blocks: Vec<_> = blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Text)
            .collect();
        assert_eq!(
            text_blocks.len(),
            3,
            "expected 3 Text blocks: {:?}",
            text_blocks
        );

        let intro = blocks.iter().find(|b| b.text == "Intro under A.").unwrap();
        assert_eq!(
            heading_path_from_blocks(&blocks, intro.seq),
            vec!["A".to_string()]
        );

        let under_b = blocks.iter().find(|b| b.text == "Text under B.").unwrap();
        assert_eq!(
            heading_path_from_blocks(&blocks, under_b.seq),
            vec!["A".to_string(), "B".to_string()]
        );

        let under_c = blocks.iter().find(|b| b.text == "Text under C.").unwrap();
        assert_eq!(
            heading_path_from_blocks(&blocks, under_c.seq),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
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
        assert_eq!(
            table_b.text, "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |",
            "table text must be reconstructed pipe-syntax Markdown"
        );
    }

    /// Table block text is valid pipe syntax: header line, separator line, one
    /// line per data row — even with inline formatting and escaped pipes in
    /// cells. This is the contract chunk_table's row splitter relies on.
    #[test]
    fn table_block_text_is_reparseable_markdown() {
        let md = "| Name | Notes |\n|---|---|\n| `a\\|b` | uses *pipes* |\n| c | d |\n";
        let blocks = markdown_to_blocks(md);
        let table_b = blocks
            .iter()
            .find(|b| matches!(&b.kind, BlockKind::Table { .. }))
            .expect("expected a table block");
        let lines: Vec<&str> = table_b.text.lines().collect();
        assert_eq!(lines.len(), 4, "header + separator + 2 rows: {lines:?}");
        assert!(lines[0].starts_with('|') && lines[0].ends_with('|'));
        assert_eq!(lines[1], "| --- | --- |");
        assert!(
            lines[2].contains("a\\|b"),
            "pipe inside a cell must be re-escaped: {}",
            lines[2]
        );
        // Every line has the same unescaped-pipe cell count.
        for line in &lines {
            let cells = line.replace("\\|", "\u{0}").matches('|').count();
            assert_eq!(cells, 3, "expected 2 cells per line: {line}");
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
        // The heading stays discrete; the HTML block merges with the
        // following paragraph into a single running-text Text block since
        // nothing structural separates them.
        assert_eq!(
            blocks.len(),
            2,
            "expected heading + one merged Text block, got {}: {:?}",
            blocks.len(),
            blocks.iter().map(|b| b.kind.kind_str()).collect::<Vec<_>>()
        );
        assert!(matches!(blocks[0].kind, BlockKind::Heading { level: 1 }));
        assert_eq!(blocks[1].kind, BlockKind::Text);
        assert!(blocks[1].text.contains("After paragraph."));
    }

    // -----------------------------------------------------------------------
    // Page stamping (#103)
    // -----------------------------------------------------------------------

    /// With empty `page_starts`, the `_with_pages` variant is byte-identical to
    /// `markdown_to_blocks` — the zero-page regression guard.
    #[test]
    fn with_pages_empty_equals_markdown_to_blocks() {
        let samples = [
            "# H1\n\npara1\n\n## H2\n\npara2\n",
            "---\ntitle: x\n---\n\n# C\n\ntext\n",
            "| A | B |\n|---|---|\n| 1 | 2 |\n",
            "para\n\n```rust\nfn f(){}\n```\n\nmore\n",
            "",
        ];
        for md in samples {
            assert_eq!(
                markdown_to_blocks_with_pages(md, &[]),
                markdown_to_blocks(md),
                "empty page_starts must match plain markdown_to_blocks for {md:?}"
            );
        }
    }

    /// Every block gets stamped with the page containing its first byte.
    #[test]
    fn blocks_stamped_with_correct_page() {
        // "# One\n\nAlpha body.\n\n"  -> offsets 0..20
        // "# Two\n\nBravo body.\n\n"  starts at 20
        // "# Three\n\nCharlie body.\n" starts at 40
        let md = "# One\n\nAlpha body.\n\n# Two\n\nBravo body.\n\n# Three\n\nCharlie body.\n";
        let p2 = md.find("# Two").unwrap();
        let p3 = md.find("# Three").unwrap();
        let page_starts = vec![(0usize, 1u32), (p2, 2u32), (p3, 3u32)];

        let blocks = markdown_to_blocks_with_pages(md, &page_starts);
        let page_of = |needle: &str| {
            blocks
                .iter()
                .find(|b| b.text.contains(needle))
                .and_then(|b| b.location.as_ref())
                .and_then(|l| l.page)
        };
        assert_eq!(page_of("One"), Some(1));
        assert_eq!(page_of("Alpha body."), Some(1));
        assert_eq!(page_of("Two"), Some(2));
        assert_eq!(page_of("Bravo body."), Some(2));
        assert_eq!(page_of("Three"), Some(3));
        assert_eq!(page_of("Charlie body."), Some(3));
    }

    /// A block is attributed to the page of its FIRST byte even when its text
    /// spans a page boundary — the coarse-`Text` packing rule (#158).
    #[test]
    fn block_spanning_page_boundary_takes_first_byte_page() {
        // A single running-text block (two paragraphs, no structural break)
        // whose second paragraph starts after the page-2 offset. The whole
        // Text block must still be attributed to page 1.
        let md = "First paragraph on page one.\n\nSecond paragraph on page two.\n";
        let boundary = md.find("Second").unwrap();
        let page_starts = vec![(0usize, 1u32), (boundary, 2u32)];

        let blocks = markdown_to_blocks_with_pages(md, &page_starts);
        assert_eq!(blocks.len(), 1, "both paragraphs fold into one Text block");
        assert_eq!(blocks[0].kind, BlockKind::Text);
        assert!(blocks[0].text.contains("Second paragraph"));
        assert_eq!(
            blocks[0].location.as_ref().and_then(|l| l.page),
            Some(1),
            "the crossing block takes the page of its first byte"
        );
    }

    /// An offset before the first `page_starts` entry attributes to the first
    /// listed page (page 1 may contribute no content, so its start offset can
    /// be > 0).
    #[test]
    fn offset_before_first_page_start_uses_first_page() {
        let md = "Intro line.\n\n# Heading\n\nBody.\n";
        // First recorded page starts at the heading, not at offset 0.
        let heading = md.find("# Heading").unwrap();
        let page_starts = vec![(heading, 5u32), (md.find("Body").unwrap(), 6u32)];

        let blocks = markdown_to_blocks_with_pages(md, &page_starts);
        let intro = blocks.iter().find(|b| b.text.contains("Intro")).unwrap();
        assert_eq!(
            intro.location.as_ref().and_then(|l| l.page),
            Some(5),
            "content before the first page start attributes to the first page"
        );
    }

    /// #103 fingerprint: a repagination (same text/kinds, different page)
    /// changes `compute_blocks_hash`, so the skip-check re-indexes instead of
    /// leaving citations on stale pages.
    #[test]
    fn blocks_hash_changes_with_page() {
        let block = |page: Option<u32>| Block {
            seq: 0,
            kind: BlockKind::Text,
            text: "Identical body text.".to_string(),
            location: page.map(|p| BlockLocation {
                page: Some(p),
                ..Default::default()
            }),
        };
        let h1 = compute_blocks_hash(&[block(Some(1))]);
        let h2 = compute_blocks_hash(&[block(Some(2))]);
        assert_ne!(h1, h2, "moving a block to a new page must change the hash");
    }

    /// A block with no page (every non-paginated format) hashes identically to
    /// before page folding — the suffix is added only when a page is present,
    /// so there is no spurious global reindex.
    #[test]
    fn blocks_hash_unchanged_without_page() {
        let paged = Block {
            seq: 0,
            kind: BlockKind::Text,
            text: "Body.".to_string(),
            location: Some(BlockLocation {
                page: None,
                ..Default::default()
            }),
        };
        let no_loc = Block {
            seq: 0,
            kind: BlockKind::Text,
            text: "Body.".to_string(),
            location: None,
        };
        // A location with page=None contributes no suffix, matching a block
        // with no location at all and the pre-#103 `{kind}:{text}` hash.
        assert_eq!(
            compute_blocks_hash(&[paged]),
            compute_blocks_hash(&[no_loc])
        );
        assert_eq!(
            compute_blocks_hash(&[no_loc_block("Body.")]),
            content_hash("text:Body.")
        );
    }

    fn no_loc_block(text: &str) -> Block {
        Block {
            seq: 0,
            kind: BlockKind::Text,
            text: text.to_string(),
            location: None,
        }
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
