//! Chunking logic for the ingestion pipeline.
//!
//! Implements the per-source-kind presets from specs/04-search-pipeline.md §3:
//! - `prose` (default): split on heading boundaries, pack to ~400 tokens with ~60 overlap
//! - `code`: structural (function/item-level) or line blocks, target ~60 lines
//! - `messages` (reserved): not implemented in v1
//!
//! Chunks reference spans in the normalized document text (as produced by the
//! `extract` crate).

use crate::ids::{chunk_id, ContentId};
use crate::types::{Block, BlockKind, Span};
use crate::Error;

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

// ---------------------------------------------------------------------------
// ChunkOutput — one chunk produced by a chunker
// ---------------------------------------------------------------------------

/// A single chunk produced by the chunker.
///
/// The `id` is content-addressed; the `text` and `span` refer to the normalized
/// document text. `heading_path` is inherited from the blocks that compose the chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkOutput {
    /// Content-addressed chunk ID: `blake3(document_id || text || span)`.
    pub id: ContentId,
    /// Chunk text.
    pub text: String,
    /// Byte range in the normalized document text.
    pub span: Span,
    /// Heading path inherited from the first block in this chunk.
    pub heading_path: Vec<String>,
}

// ---------------------------------------------------------------------------
// Chunker preset configuration
// ---------------------------------------------------------------------------

/// Configuration for the chunking operation.
#[derive(Debug, Clone)]
pub struct ChunkerConfig {
    /// Preset name: "prose", "code", or "messages" (reserved).
    pub preset: String,
    /// Target chunk size in characters. `None` = preset default.
    pub target_chars: Option<usize>,
    /// Overlap in characters. `None` = preset default.
    pub overlap_chars: Option<usize>,
}

impl ChunkerConfig {
    /// Create a config for the `prose` preset with the spec defaults.
    ///
    /// Default target ≈ 1600 chars (≈400 tokens at ~4 chars/token), overlap ≈ 240 chars (≈60 tokens).
    pub fn prose() -> Self {
        Self {
            preset: "prose".to_string(),
            target_chars: Some(1600),
            overlap_chars: Some(240),
        }
    }

    /// Create a config for the `code` preset with the spec defaults.
    ///
    /// Target ≈ 60 lines (≈3000 chars assuming ~50 chars/line average).
    pub fn code() -> Self {
        Self {
            preset: "code".to_string(),
            target_chars: Some(3000),
            overlap_chars: Some(0),
        }
    }

    /// Create a `ChunkerConfig` from a preset name string.
    ///
    /// Returns `Error::InvalidRequest` for unknown presets.
    pub fn from_preset(preset: &str) -> Result<Self, Error> {
        match preset {
            "prose" => Ok(Self::prose()),
            "code" => Ok(Self::code()),
            "messages" => Err(Error::InvalidRequest {
                message: "chunking preset 'messages' is reserved and not implemented in v1; \
                          use 'prose' or 'code'"
                    .to_string(),
            }),
            other => Err(Error::InvalidRequest {
                message: format!(
                    "unknown chunking preset '{}'; valid values: prose, code",
                    other
                ),
            }),
        }
    }

    /// Resolved target chars (uses preset default if not overridden).
    pub fn resolved_target_chars(&self) -> usize {
        self.target_chars.unwrap_or(1600)
    }

    /// Resolved overlap chars (uses preset default if not overridden).
    pub fn resolved_overlap_chars(&self) -> usize {
        self.overlap_chars.unwrap_or(240)
    }
}

// ---------------------------------------------------------------------------
// Main chunk function
// ---------------------------------------------------------------------------

/// Chunk a document's blocks into `ChunkOutput` records.
///
/// The `document_id` is used to derive chunk IDs (content-addressed).
/// The `full_text` is the normalized document text returned by the extractor.
///
/// Respects preset and per-document configuration.
///
/// # Errors
/// - Returns `Error::InvalidRequest` if the preset is unknown or reserved.
pub fn chunk_document(
    document_id: &str,
    full_text: &str,
    blocks: &[Block],
    config: &ChunkerConfig,
) -> Result<Vec<ChunkOutput>, Error> {
    match config.preset.as_str() {
        "prose" => chunk_prose(document_id, full_text, blocks, config),
        "code" => chunk_code(document_id, full_text, blocks, config),
        "messages" => Err(Error::InvalidRequest {
            message: "chunking preset 'messages' is reserved and not implemented in v1; \
                      use 'prose' or 'code'"
                .to_string(),
        }),
        other => Err(Error::InvalidRequest {
            message: format!(
                "unknown chunking preset '{}'; valid values: prose, code",
                other
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Prose chunker
// ---------------------------------------------------------------------------

/// Prose chunker: split on heading boundaries, pack blocks to target size with overlap.
///
/// Strategy:
/// 1. Iterate blocks; each heading starts a new "segment".
/// 2. Accumulate block texts into a running buffer (tracked as a span in full_text).
/// 3. When the buffer exceeds `target_chars`, flush it as one or more chunks.
/// 4. Each emitted chunk overlaps the previous by `overlap_chars`.
fn chunk_prose(
    document_id: &str,
    full_text: &str,
    blocks: &[Block],
    config: &ChunkerConfig,
) -> Result<Vec<ChunkOutput>, Error> {
    let target = config.resolved_target_chars();
    let overlap = config.resolved_overlap_chars();

    if blocks.is_empty() {
        // If no blocks but there is text, make a single chunk of the whole text
        if !full_text.is_empty() {
            let span = Span::new(0, full_text.len());
            let id = chunk_id(document_id, full_text, 0, full_text.len());
            return Ok(vec![ChunkOutput {
                id,
                text: full_text.to_string(),
                span,
                heading_path: vec![],
            }]);
        }
        return Ok(vec![]);
    }

    let mut chunks = Vec::new();

    // Accumulator: span start/end + heading path.
    //
    // `buf_heading` tracks the heading path at the time the *first* block was
    // added to the current buffer.  We must record this separately so that
    // overlap content carried forward from a previous section does not inherit
    // the *new* section's heading path.
    let mut buf_start: usize = blocks[0].span.start.min(full_text.len());
    let mut buf_end: usize = buf_start;
    // heading of the first block added to the current buffer
    let mut buf_heading: Vec<String> = blocks[0].heading_path.clone();
    // heading that the next block (or overlap start) should inherit
    let mut current_heading: Vec<String> = blocks[0].heading_path.clone();
    // whether the buffer is currently empty (no block content added yet)
    let mut buf_empty: bool = true;

    for block in blocks {
        let blk_start = block.span.start.min(full_text.len());
        let blk_end = block.span.end.min(full_text.len());

        if blk_start >= blk_end {
            continue;
        }

        // Heading starts a new section — flush if buffer is non-trivially large.
        if block.kind == BlockKind::Heading && buf_end > buf_start {
            let current_size = buf_end - buf_start;
            if current_size >= target / 2 {
                // Flush current buffer using the heading recorded when this
                // buffer started being filled.
                emit_span_chunks(
                    document_id,
                    full_text,
                    buf_start,
                    buf_end,
                    &buf_heading,
                    target,
                    overlap,
                    &mut chunks,
                );
                // Start new buffer with overlap from previous section.
                // The overlap content logically belongs to the *new* section,
                // so buf_heading will be updated when the first real block is
                // added below.
                let overlap_start = buf_end.saturating_sub(overlap);
                let overlap_start = floor_char_boundary(full_text, overlap_start);
                buf_start = overlap_start;
                buf_end = overlap_start;
                buf_empty = true;
            }
            // Update the heading that will be applied to the next buffer fill.
            current_heading = block.heading_path.clone();
        }

        // Extend buffer.
        if buf_empty {
            // First content block added to this buffer: record the heading path
            // that was active when this buffer was started.
            buf_heading = current_heading.clone();
            buf_empty = false;
        }
        if buf_start > blk_start {
            buf_start = blk_start;
        }
        buf_end = blk_end.max(buf_end);

        // Flush if buffer exceeds target.
        if buf_end - buf_start >= target {
            emit_span_chunks(
                document_id,
                full_text,
                buf_start,
                buf_end,
                &buf_heading,
                target,
                overlap,
                &mut chunks,
            );
            let overlap_start = buf_end.saturating_sub(overlap);
            let overlap_start = floor_char_boundary(full_text, overlap_start);
            buf_start = overlap_start;
            buf_end = overlap_start;
            buf_empty = true;
        }
    }

    // Flush remaining.
    if buf_end > buf_start {
        emit_span_chunks(
            document_id,
            full_text,
            buf_start,
            buf_end,
            &buf_heading,
            target,
            overlap,
            &mut chunks,
        );
    }

    Ok(chunks)
}

/// Chunk parameters bundled to reduce function argument count.
struct SpanChunkParams<'a> {
    document_id: &'a str,
    full_text: &'a str,
    heading_path: &'a [String],
    target: usize,
    overlap: usize,
}

/// Emit one or more chunks for the span [start..end] of full_text.
///
/// If the span fits within target, emits a single chunk. Otherwise, splits
/// at word/sentence boundaries, with overlap.
#[allow(clippy::too_many_arguments)]
fn emit_span_chunks(
    document_id: &str,
    full_text: &str,
    start: usize,
    end: usize,
    heading_path: &[String],
    target: usize,
    overlap: usize,
    chunks: &mut Vec<ChunkOutput>,
) {
    let params = SpanChunkParams {
        document_id,
        full_text,
        heading_path,
        target,
        overlap,
    };
    emit_span_chunks_inner(&params, start, end, chunks);
}

fn emit_span_chunks_inner(
    params: &SpanChunkParams<'_>,
    start: usize,
    end: usize,
    chunks: &mut Vec<ChunkOutput>,
) {
    // Clamp to text length and align to char boundaries to avoid panics on
    // multi-byte UTF-8 sequences (A1 fix).
    let start = floor_char_boundary(params.full_text, start.min(params.full_text.len()));
    let end = floor_char_boundary(params.full_text, end.min(params.full_text.len()));
    if start >= end {
        return;
    }

    let text = &params.full_text[start..end];

    if text.len() <= params.target {
        let id = chunk_id(params.document_id, text, start, end);
        chunks.push(ChunkOutput {
            id,
            text: text.to_string(),
            span: Span::new(start, end),
            heading_path: params.heading_path.to_vec(),
        });
        return;
    }

    // Split into sub-chunks with overlap.
    let mut pos = start;
    while pos < end {
        let remaining = end - pos;
        let split_len = remaining.min(params.target);
        let split_end = find_split_point(params.full_text, pos, split_len);
        // Align split_end to a char boundary (A1 fix).
        let split_end = floor_char_boundary(params.full_text, split_end.min(end));

        if split_end <= pos {
            break;
        }

        let chunk_text = &params.full_text[pos..split_end];
        let id = chunk_id(params.document_id, chunk_text, pos, split_end);
        chunks.push(ChunkOutput {
            id,
            text: chunk_text.to_string(),
            span: Span::new(pos, split_end),
            heading_path: params.heading_path.to_vec(),
        });

        if split_end >= end {
            break;
        }

        // Advance with overlap; align to char boundary (A1 fix).
        let next_start = split_end.saturating_sub(params.overlap);
        let next_start = floor_char_boundary(params.full_text, next_start);
        let next_start = find_word_boundary_forward(params.full_text, next_start);
        if next_start >= split_end {
            // Avoid infinite loop.
            pos = split_end;
        } else {
            pos = next_start.max(pos + 1);
        }
    }
}

/// Find a good split point at or before `start + target`, preferring sentence/paragraph breaks.
fn find_split_point(text: &str, start: usize, target: usize) -> usize {
    // Align both start and end to char boundaries to avoid panics on multi-byte
    // UTF-8 sequences (A1 fix).
    let start = floor_char_boundary(text, start.min(text.len()));
    let end = floor_char_boundary(text, (start + target).min(text.len()));
    if end == text.len() {
        return end;
    }

    let slice = &text[start..end];

    // Try to split at a paragraph boundary (\n\n)
    if let Some(pos) = slice.rfind("\n\n") {
        return start + pos + 2;
    }

    // Try to split at a sentence boundary ('. ', '! ', '? ')
    for pat in &[". ", "! ", "? "] {
        if let Some(pos) = slice.rfind(pat) {
            return start + pos + pat.len();
        }
    }

    // Try to split at a newline
    if let Some(pos) = slice.rfind('\n') {
        return start + pos + 1;
    }

    // Try to split at a space
    if let Some(pos) = slice.rfind(' ') {
        return start + pos + 1;
    }

    // Fall back to hard cut
    end
}

/// Find a word boundary at or after `pos` in `text`.
fn find_word_boundary_forward(text: &str, pos: usize) -> usize {
    // Align to char boundary first to avoid panics on multi-byte sequences (A1).
    let pos = floor_char_boundary(text, pos.min(text.len()));
    if pos >= text.len() {
        return pos;
    }
    if text.as_bytes().get(pos) == Some(&b' ') {
        return pos + 1;
    }
    // Walk forward to find a space.
    text[pos..].find(' ').map(|i| pos + i + 1).unwrap_or(pos)
}

// ---------------------------------------------------------------------------
// Code chunker
// ---------------------------------------------------------------------------

/// Code chunker: line-based chunking targeting ~60 lines per chunk.
///
/// Splits at blank lines or after `target_chars` worth of content.
fn chunk_code(
    document_id: &str,
    full_text: &str,
    blocks: &[Block],
    config: &ChunkerConfig,
) -> Result<Vec<ChunkOutput>, Error> {
    let target = config.resolved_target_chars();

    if blocks.is_empty() {
        if !full_text.is_empty() {
            let id = chunk_id(document_id, full_text, 0, full_text.len());
            return Ok(vec![ChunkOutput {
                id,
                text: full_text.to_string(),
                span: Span::new(0, full_text.len()),
                heading_path: vec![],
            }]);
        }
        return Ok(vec![]);
    }

    // For code: group by code-block boundaries, then by size
    let mut chunks = Vec::new();
    let mut current_start = blocks[0].span.start;
    let mut current_end = blocks[0].span.start;
    let mut current_heading = blocks[0].heading_path.clone();

    for block in blocks {
        let block_end = block.span.end.min(full_text.len());
        let block_start = block.span.start.min(full_text.len());

        if block_start >= block_end {
            continue;
        }

        let current_size = current_end.saturating_sub(current_start);

        // Flush if adding this block would exceed target.
        if current_size > 0 && current_size + (block_end - block_start) > target {
            // Align to char boundaries (A1 fix).
            let cs = floor_char_boundary(full_text, current_start);
            let ce = floor_char_boundary(full_text, current_end);
            let span = Span::new(cs, ce);
            let chunk_text = &full_text[cs..ce];
            let id = chunk_id(document_id, chunk_text, cs, ce);
            chunks.push(ChunkOutput {
                id,
                text: chunk_text.to_string(),
                span,
                heading_path: current_heading.clone(),
            });
            current_start = block_start;
            current_heading = block.heading_path.clone();
        }

        current_end = block_end;
        if current_start > block_start {
            current_start = block_start;
        }
    }

    // Flush remaining.
    if current_end > current_start {
        // Align to char boundaries (A1 fix).
        let cs = floor_char_boundary(full_text, current_start);
        let ce = floor_char_boundary(full_text, current_end);
        let span = Span::new(cs, ce);
        let chunk_text = &full_text[cs..ce];
        let id = chunk_id(document_id, chunk_text, cs, ce);
        chunks.push(ChunkOutput {
            id,
            text: chunk_text.to_string(),
            span,
            heading_path: current_heading,
        });
    }

    Ok(chunks)
}

// ---------------------------------------------------------------------------
// Tests (TDD: failing first)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::document_id;
    use crate::types::{Block, BlockKind, Span};

    fn make_block(ordinal: usize, kind: BlockKind, text: &str, start: usize) -> Block {
        Block {
            document_id: String::new(),
            ordinal,
            kind,
            text: text.to_string(),
            span: Span::new(start, start + text.len()),
            heading_path: vec![],
        }
    }

    fn make_heading_block(ordinal: usize, text: &str, start: usize, path: Vec<String>) -> Block {
        Block {
            document_id: String::new(),
            ordinal,
            kind: BlockKind::Heading,
            text: text.to_string(),
            span: Span::new(start, start + text.len()),
            heading_path: path,
        }
    }

    fn make_para_block(ordinal: usize, text: &str, start: usize, path: Vec<String>) -> Block {
        Block {
            document_id: String::new(),
            ordinal,
            kind: BlockKind::Paragraph,
            text: text.to_string(),
            span: Span::new(start, start + text.len()),
            heading_path: path,
        }
    }

    // ---------------------------------------------------------------------------
    // ChunkerConfig tests
    // ---------------------------------------------------------------------------

    #[test]
    fn chunker_config_prose_defaults() {
        let cfg = ChunkerConfig::prose();
        assert_eq!(cfg.preset, "prose");
        assert_eq!(cfg.resolved_target_chars(), 1600);
        assert_eq!(cfg.resolved_overlap_chars(), 240);
    }

    #[test]
    fn chunker_config_code_defaults() {
        let cfg = ChunkerConfig::code();
        assert_eq!(cfg.preset, "code");
        assert_eq!(cfg.resolved_target_chars(), 3000);
    }

    #[test]
    fn chunker_config_from_preset_prose() {
        let cfg = ChunkerConfig::from_preset("prose").unwrap();
        assert_eq!(cfg.preset, "prose");
    }

    #[test]
    fn chunker_config_from_preset_code() {
        let cfg = ChunkerConfig::from_preset("code").unwrap();
        assert_eq!(cfg.preset, "code");
    }

    #[test]
    fn chunker_config_from_preset_messages_errors() {
        let result = ChunkerConfig::from_preset("messages");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "invalid_request");
    }

    #[test]
    fn chunker_config_from_preset_unknown_errors() {
        let result = ChunkerConfig::from_preset("unknown_preset");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "invalid_request");
    }

    // ---------------------------------------------------------------------------
    // Prose chunker tests
    // ---------------------------------------------------------------------------

    #[test]
    fn prose_chunk_empty_document_returns_empty() {
        let doc_id = document_id("file:///test.md", "abc123");
        let cfg = ChunkerConfig::prose();
        let result = chunk_document(&doc_id, "", &[], &cfg).unwrap();
        assert!(result.is_empty(), "empty doc should produce no chunks");
    }

    #[test]
    fn prose_chunk_single_block() {
        let full_text = "Hello, this is a paragraph.";
        let blocks = vec![make_block(0, BlockKind::Paragraph, full_text, 0)];
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg).unwrap();
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        assert!(
            chunks.iter().any(|c| c.text.contains("Hello")),
            "chunk should contain the paragraph text"
        );
    }

    #[test]
    fn prose_chunk_span_references_full_text() {
        let full_text = "# Introduction\n\nThis is the intro paragraph.";
        let blocks = vec![
            make_heading_block(0, "Introduction", 0, vec!["Introduction".to_string()]),
            make_para_block(
                1,
                "This is the intro paragraph.",
                16,
                vec!["Introduction".to_string()],
            ),
        ];
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg).unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            // Span must be valid
            assert!(
                chunk.span.end <= full_text.len(),
                "span end must be within text"
            );
            assert!(chunk.span.start <= chunk.span.end, "span start <= end");
            // Text at the span position must be non-empty
            let span_text = &full_text[chunk.span.start..chunk.span.end];
            assert!(!span_text.is_empty(), "span must reference non-empty text");
        }
    }

    #[test]
    fn prose_chunk_large_text_splits_into_multiple_chunks() {
        // Create a text larger than the target (1600 chars)
        let para = "A".repeat(500) + " ";
        let full_text = para.repeat(5); // 2500+ chars
        let blocks: Vec<Block> = (0..5)
            .map(|i| {
                let start = i * 501;
                let end = (start + 500).min(full_text.len());
                Block {
                    document_id: String::new(),
                    ordinal: i,
                    kind: BlockKind::Paragraph,
                    text: "A".repeat(500),
                    span: Span::new(start, end),
                    heading_path: vec![],
                }
            })
            .collect();

        let doc_id = document_id("file:///large.md", "hash");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_document(&doc_id, &full_text, &blocks, &cfg).unwrap();
        assert!(
            chunks.len() >= 2,
            "large document should produce multiple chunks, got {}",
            chunks.len()
        );
    }

    #[test]
    fn prose_chunk_ids_are_content_addressed() {
        let full_text = "Hello world this is content.";
        let blocks = vec![make_block(0, BlockKind::Paragraph, full_text, 0)];
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks1 = chunk_document(&doc_id, full_text, &blocks, &cfg).unwrap();
        let chunks2 = chunk_document(&doc_id, full_text, &blocks, &cfg).unwrap();

        assert_eq!(chunks1.len(), chunks2.len());
        for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(c1.id, c2.id, "chunk IDs must be deterministic");
        }
    }

    #[test]
    fn prose_chunk_heading_path_inherited() {
        let full_text = "# API\n\nAPI documentation.\n\n# Auth\n\nAuth documentation.";
        let blocks = vec![
            make_heading_block(0, "API", 0, vec!["API".to_string()]),
            make_para_block(1, "API documentation.", 7, vec!["API".to_string()]),
            make_heading_block(2, "Auth", 27, vec!["Auth".to_string()]),
            make_para_block(3, "Auth documentation.", 34, vec!["Auth".to_string()]),
        ];
        let doc_id = document_id("file:///api.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg).unwrap();
        assert!(!chunks.is_empty());
        // At least one chunk should have a heading path
        let with_path: Vec<_> = chunks
            .iter()
            .filter(|c| !c.heading_path.is_empty())
            .collect();
        assert!(
            !with_path.is_empty(),
            "at least one chunk should have heading_path"
        );
    }

    #[test]
    fn prose_chunk_messages_preset_errors() {
        let full_text = "Hello.";
        let blocks = vec![make_block(0, BlockKind::Paragraph, full_text, 0)];
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_chars: None,
            overlap_chars: None,
        };

        let result = chunk_document(&doc_id, full_text, &blocks, &cfg);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "invalid_request");
    }

    // ---------------------------------------------------------------------------
    // Code chunker tests
    // ---------------------------------------------------------------------------

    #[test]
    fn code_chunk_single_block() {
        let full_text = "fn hello() {\n    println!(\"hi\");\n}";
        let blocks = vec![make_block(0, BlockKind::Code, full_text, 0)];
        let doc_id = document_id("file:///lib.rs", "abc");
        let cfg = ChunkerConfig::code();

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, full_text);
    }

    #[test]
    fn code_chunk_large_splits() {
        // Create a large code block exceeding 3000 chars
        let line = "let x = some_function_with_long_name(arg1, arg2, arg3);\n";
        let full_text = line.repeat(100); // ~5600 chars
        let blocks: Vec<Block> = (0..10)
            .map(|i| {
                let start = i * (full_text.len() / 10);
                let end = ((i + 1) * (full_text.len() / 10)).min(full_text.len());
                Block {
                    document_id: String::new(),
                    ordinal: i,
                    kind: BlockKind::Code,
                    text: full_text[start..end].to_string(),
                    span: Span::new(start, end),
                    heading_path: vec![],
                }
            })
            .collect();

        let doc_id = document_id("file:///lib.rs", "hash");
        let cfg = ChunkerConfig::code();

        let chunks = chunk_document(&doc_id, &full_text, &blocks, &cfg).unwrap();
        assert!(
            chunks.len() >= 2,
            "large code file should produce multiple chunks"
        );
    }

    // ---------------------------------------------------------------------------
    // find_split_point tests
    // ---------------------------------------------------------------------------

    #[test]
    fn find_split_point_at_end() {
        let text = "hello world";
        let result = find_split_point(text, 0, 100);
        assert_eq!(result, text.len());
    }

    #[test]
    fn find_split_point_at_paragraph() {
        let text = "First paragraph.\n\nSecond paragraph.";
        let result = find_split_point(text, 0, 25);
        // Should split at \n\n
        assert!(result <= 20, "should split before second paragraph");
        assert!(result > 0);
    }

    // ---------------------------------------------------------------------------
    // A1 — Multi-byte UTF-8 must not panic
    // ---------------------------------------------------------------------------

    #[test]
    fn chunk_document_with_multibyte_utf8_does_not_panic() {
        // Japanese, Cyrillic, and em-dash — all multi-byte characters.
        let text = "こんにちは world — это тест";
        let doc_id = "doc-multibyte";
        let blocks = vec![make_block(0, BlockKind::Paragraph, text, 0)];
        let result = chunk_document(doc_id, text, &blocks, &ChunkerConfig::prose());
        assert!(
            result.is_ok(),
            "chunking multi-byte text should not panic: {:?}",
            result.err()
        );
    }

    #[test]
    fn chunk_document_multibyte_code_preset_does_not_panic() {
        // Long repeated multi-byte text to force splitting within multi-byte sequences.
        let unit = "日本語テキスト: これはテストです。 ";
        let text = unit.repeat(200); // large enough to trigger splits
        let doc_id = "doc-multibyte-code";
        // Build blocks that reference the text by byte offsets.
        // All boundaries are aligned to char boundaries to build valid input.
        let block_size_approx = text.len() / 4;
        let mut blocks = Vec::new();
        let mut prev_end = 0usize;
        for i in 0..4 {
            let raw_end = if i < 3 {
                (i + 1) * block_size_approx
            } else {
                text.len()
            };
            let start = floor_char_boundary(text.as_str(), prev_end);
            let end = floor_char_boundary(text.as_str(), raw_end.min(text.len()));
            if start >= end {
                continue;
            }
            blocks.push(Block {
                document_id: String::new(),
                ordinal: i,
                kind: BlockKind::Code,
                text: text[start..end].to_string(),
                span: Span::new(start, end),
                heading_path: vec![],
            });
            prev_end = end;
        }
        let result = chunk_document(doc_id, &text, &blocks, &ChunkerConfig::code());
        assert!(
            result.is_ok(),
            "code chunking multi-byte text should not panic: {:?}",
            result.err()
        );
    }

    // ---------------------------------------------------------------------------
    // B1 — heading_path must reflect the section the chunk content belongs to,
    //       not the heading that was current when the buffer was flushed.
    // ---------------------------------------------------------------------------

    #[test]
    fn prose_chunk_heading_path_correct_after_section_flush() {
        // Build a document large enough to force a flush at the heading boundary.
        // Section A has enough content to trigger a flush; section B should then
        // get its own heading path.
        let section_a_para = "A".repeat(900); // enough to push buf_end > target/2 (800)
        let section_b_para = "B".repeat(100);

        // Manually build full_text with offsets matching the block spans.
        //   "# Section A\n" + section_a_para + "\n# Section B\n" + section_b_para
        let heading_a = "# Section A\n";
        let heading_b = "\n# Section B\n";
        let full_text = format!(
            "{}{}{}{}",
            heading_a, section_a_para, heading_b, section_b_para
        );

        let ha_end = heading_a.len();
        let pa_end = ha_end + section_a_para.len();
        let hb_end = pa_end + heading_b.len();
        let _pb_end = hb_end + section_b_para.len();

        let blocks = vec![
            make_heading_block(0, "Section A", 0, vec!["Section A".to_string()]),
            make_para_block(1, &section_a_para, ha_end, vec!["Section A".to_string()]),
            make_heading_block(2, "Section B", pa_end, vec!["Section B".to_string()]),
            make_para_block(3, &section_b_para, hb_end, vec!["Section B".to_string()]),
        ];

        let doc_id = "doc-heading-path";
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_chars: Some(1600),
            overlap_chars: Some(0), // no overlap to simplify assertions
        };

        let chunks = chunk_document(doc_id, &full_text, &blocks, &cfg).unwrap();
        assert!(!chunks.is_empty(), "should produce at least one chunk");

        // The last chunk should contain section B content and carry "Section B".
        let last = chunks.last().unwrap();
        assert!(
            last.text.contains('B'),
            "last chunk should contain section B content"
        );
        assert_eq!(
            last.heading_path,
            vec!["Section B".to_string()],
            "last chunk heading_path must be 'Section B', not a stale heading"
        );
    }
}
