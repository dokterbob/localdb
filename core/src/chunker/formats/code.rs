//! Code chunker (interim): line-based text packer over the Markdown string.

use crate::block::{Block, BlockKind};
use crate::chunker::output::{finalize_ids, ChunkOutput};
use crate::chunker::ChunkerConfig;
use crate::types::Span;
use crate::Error;

use super::{chunk_each, ChunkContext, FormatChunker, GroupScope};

/// Returns the largest byte index ≤ `index` that is a valid UTF-8 char boundary.
/// MSRV-safe replacement for `str::floor_char_boundary` (stable since 1.91).
#[inline]
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let index = index.min(s.len());
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Accumulates placeholder chunks for a single block: owns the growing output `Vec` plus the
/// running `seq_in_block` counter, so callers thread one value instead of a `Vec` and a counter
/// separately. `block_seq` is fixed for the sink's lifetime.
struct ChunkSink<'a> {
    block_seq: u32,
    seq_in_block: u32,
    chunks: &'a mut Vec<ChunkOutput>,
}

impl ChunkSink<'_> {
    /// Push a placeholder chunk for `text`/`span` and advance `seq_in_block`.
    fn push(&mut self, text: &str, span: Span) {
        self.chunks.push(ChunkOutput::placeholder(
            text.to_string(),
            span,
            vec![],
            self.block_seq,
            self.seq_in_block,
        ));
        self.seq_in_block += 1;
    }
}

/// Flush the byte range `[start, end)` of `markdown` into a placeholder chunk via `sink`,
/// snapping both ends to the nearest UTF-8 char boundary and skipping the push if the snapped
/// range is empty.
fn flush_range(markdown: &str, start: usize, end: usize, sink: &mut ChunkSink<'_>) {
    let cs = floor_char_boundary(markdown, start);
    let ce = floor_char_boundary(markdown, end);
    if cs < ce {
        sink.push(&markdown[cs..ce], Span::new(cs, ce));
    }
}

/// Split the overlong line `markdown[line_start..line_end]` into `target`-char pieces via
/// `sink`, preferring to land the cut on whitespace rather than mid-word (#191).
fn split_overlong_line(
    markdown: &str,
    line_start: usize,
    line_end: usize,
    target: usize,
    sink: &mut ChunkSink<'_>,
) {
    let mut pos = line_start;
    while pos < line_end {
        let slice = &markdown[pos..line_end];
        let byte_len: usize = slice
            .char_indices()
            .take(target)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(slice.len());
        if byte_len == 0 {
            break; // safety: prevent infinite loop
        }

        // Back off to the last whitespace within this window, if any. Bounded to
        // the `target`-char window already sliced above (via `byte_len`), so this
        // stays O(n) overall — each window is scanned at most once, not re-scanned
        // from the start of the line. The whitespace char is kept at the END of the
        // current piece (its length includes it), so the next piece starts clean on
        // a non-whitespace char; either attachment choice keeps the cut point off an
        // alphanumeric-alphanumeric boundary, since one side is whitespace.
        let mut cut_len = byte_len;
        let window = &slice[..byte_len];
        if let Some((ws_byte_idx, ws_ch)) =
            window.char_indices().rev().find(|(_, c)| c.is_whitespace())
        {
            let candidate_len = ws_byte_idx + ws_ch.len_utf8();
            // Only back off when the resulting piece is still substantial (> half
            // the window) — otherwise (e.g. whitespace right near the window start)
            // keep the hard char cut so pieces don't degenerate to near-empty.
            // When there's no whitespace at all (base64/URLs), this branch never
            // fires and we fall through to the hard char cut, unchanged.
            if candidate_len * 2 > byte_len {
                cut_len = candidate_len;
            }
        }

        let piece_end = (pos + cut_len).min(line_end);
        if pos < piece_end {
            sink.push(&markdown[pos..piece_end], Span::new(pos, piece_end));
        }
        pos = piece_end;
    }
}

// ---------------------------------------------------------------------------
// Code chunker (interim)
// ---------------------------------------------------------------------------

/// Code chunker: interim line-based text packer over the Markdown string.
///
/// NOTE: This is a temporary downgrade from the old block-driven code chunker.
/// It will be superseded by `text-splitter::CodeSplitter` (tree-sitter) when
/// code sources become a focus. See specs/04-search-pipeline.md §2.
pub(in crate::chunker) fn chunk_code(
    resource_id: &str,
    markdown: &str,
    config: &ChunkerConfig,
    block_seq: u32,
) -> Result<Vec<ChunkOutput>, Error> {
    if markdown.is_empty() {
        return Ok(vec![]);
    }

    let target = config.resolved_target_tokens(); // used as char budget
    let mut chunks = Vec::new();
    let mut sink = ChunkSink {
        block_seq,
        seq_in_block: 0,
        chunks: &mut chunks,
    };
    let mut current_start = 0usize;
    let mut current_end = 0usize;

    for (line_start, line) in line_offsets(markdown) {
        let line_end = line_start + line.len();

        // Hard-split overlong lines at char boundaries.
        if line.chars().count() > target {
            // Flush any pending content first.
            if current_end > current_start {
                flush_range(markdown, current_start, current_end, &mut sink);
            }

            // Split the overlong line into target-sized char pieces, preferring to land
            // the cut on whitespace rather than mid-word (#191).
            split_overlong_line(markdown, line_start, line_end, target, &mut sink);
            current_start = line_end;
            current_end = line_end;
            continue;
        }

        let current_size = current_end.saturating_sub(current_start);

        if current_size > 0 && current_size + (line_end - line_start) > target {
            flush_range(markdown, current_start, current_end, &mut sink);
            current_start = line_start;
        }

        if current_size == 0 {
            current_start = line_start;
        }
        current_end = line_end;
    }

    // Flush remaining content.
    if current_end > current_start {
        flush_range(markdown, current_start, current_end, &mut sink);
    }

    // Ids depend on `block_seq`/`seq_in_block`, both final at this point.
    finalize_ids(resource_id, &mut chunks);

    Ok(chunks)
}

/// Iterate over lines in `s`, yielding `(byte_offset_of_line_start, line_slice)`.
///
/// `split_inclusive('\n')` keeps the newline at the end of each slice, so
/// `line_start + line.len()` == start of the next line.
fn line_offsets(s: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    s.split_inclusive('\n').map(move |line| {
        let start = offset;
        offset += line.len();
        (start, line)
    })
}

// ---------------------------------------------------------------------------
// FormatChunker impl
// ---------------------------------------------------------------------------

/// `FormatChunker` for `Code` blocks, plus `Heading`/`Text` blocks when
/// `config.preset == "code"` (registry precedence puts this ahead of `Prose` in
/// [`super::FORMATS`] so that routing wins).
pub(in crate::chunker) struct Code;

impl FormatChunker for Code {
    fn name(&self) -> &'static str {
        "code"
    }

    fn scope(&self) -> GroupScope {
        GroupScope::Run
    }

    fn claims(&self, block: &Block, config: &ChunkerConfig) -> bool {
        matches!(block.kind, BlockKind::Code { .. })
            || (matches!(block.kind, BlockKind::Heading { .. } | BlockKind::Text)
                && config.preset == "code")
    }

    fn chunk(&self, ctx: &ChunkContext<'_>, blocks: &[&Block]) -> Result<Vec<ChunkOutput>, Error> {
        chunk_each(ctx, blocks, |ctx, block| {
            chunk_code(ctx.resource_id, &block.text, ctx.config, block.seq)
        })
    }
}
