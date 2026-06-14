//! Chunking logic for the ingestion pipeline.
//!
//! Implements the per-source-kind presets from specs/04-search-pipeline.md §3:
//! - `prose` (default): Markdown-structure-aware split (via `text-splitter`),
//!   token-accurate to the model tokenizer, target ~512 tokens with ~64 overlap
//! - `code`: structural (function/item-level) or line blocks, target ~60 lines
//! - `messages` (reserved): not implemented in v1
//!
//! Chunks reference spans in the normalized document text (as produced by the
//! `extract` crate).

use std::sync::Arc;

use crate::ids::{chunk_id, ContentId};
use crate::types::{Block, Span};
use crate::Error;

// ---------------------------------------------------------------------------
// ChunkSizer — pluggable size metric (tokens or chars)
// ---------------------------------------------------------------------------

/// A pluggable size metric for chunking.
///
/// `CharSizer` counts characters; `TokenSizer` wraps a model tokenizer's
/// token-counting closure. This is *our* trait (not `text-splitter`'s) so the
/// `text-splitter` dependency never leaks through the public API.
pub trait ChunkSizer: Send + Sync {
    /// Return the size of `text` in this metric's units.
    fn size(&self, text: &str) -> usize;
}

/// Sizer that counts Unicode scalar values (characters).
pub struct CharSizer;

impl ChunkSizer for CharSizer {
    fn size(&self, t: &str) -> usize {
        t.chars().count()
    }
}

/// Sizer backed by a token-counting closure (e.g. a model tokenizer).
#[derive(Clone)]
pub struct TokenSizer(Arc<dyn Fn(&str) -> usize + Send + Sync>);

impl TokenSizer {
    /// Build a `TokenSizer` from a token-counting closure.
    pub fn new(f: Arc<dyn Fn(&str) -> usize + Send + Sync>) -> Self {
        Self(f)
    }
}

impl ChunkSizer for TokenSizer {
    fn size(&self, t: &str) -> usize {
        (self.0)(t)
    }
}

/// Internal newtype bridging *our* `ChunkSizer` to `text-splitter`'s trait.
struct TsSizer<'a>(&'a dyn ChunkSizer);

impl text_splitter::ChunkSizer for TsSizer<'_> {
    fn size(&self, chunk: &str) -> usize {
        self.0.size(chunk)
    }
}

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
    /// Target chunk size in tokens. `None` = preset default.
    ///
    /// For the `prose` preset this is interpreted by the active `ChunkSizer`
    /// (tokens for `TokenSizer`, chars for `CharSizer`). For the `code` preset
    /// it is still interpreted as a character budget (hand-rolled splitter).
    pub target_tokens: Option<usize>,
    /// Overlap in tokens. `None` = preset default.
    pub overlap_tokens: Option<usize>,
}

impl ChunkerConfig {
    /// Create a config for the `prose` preset with the spec defaults.
    ///
    /// Default target ≈ 512 tokens, overlap ≈ 64 tokens.
    pub fn prose() -> Self {
        Self {
            preset: "prose".to_string(),
            target_tokens: Some(512),
            overlap_tokens: Some(64),
        }
    }

    /// Create a config for the `code` preset with the spec defaults.
    ///
    /// Target ≈ 60 lines (≈3000 chars assuming ~50 chars/line average). The
    /// code path interprets these values as character counts.
    pub fn code() -> Self {
        Self {
            preset: "code".to_string(),
            target_tokens: Some(3000),
            overlap_tokens: Some(0),
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

    /// Resolved target tokens (uses preset default if not overridden).
    pub fn resolved_target_tokens(&self) -> usize {
        self.target_tokens.unwrap_or(512)
    }

    /// Resolved overlap tokens (uses preset default if not overridden).
    pub fn resolved_overlap_tokens(&self) -> usize {
        self.overlap_tokens.unwrap_or(64)
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
/// Respects preset and per-document configuration. The `sizer` measures chunk
/// sizes for the `prose` preset (the `code` preset uses its own char budget).
///
/// # Errors
/// - Returns `Error::InvalidRequest` if the preset is unknown or reserved.
pub fn chunk_document(
    document_id: &str,
    full_text: &str,
    blocks: &[Block],
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    match config.preset.as_str() {
        "prose" => chunk_prose(document_id, full_text, blocks, config, sizer),
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

/// Prose chunker: Markdown-structure-aware split via `text-splitter`.
///
/// Strategy:
/// 1. Degenerate cases (empty text → no chunks; non-empty text with no blocks →
///    single whole-text chunk) are handled up front to match historical behaviour.
/// 2. Otherwise a `MarkdownSplitter` packs the document into chunks sized by the
///    active `ChunkSizer` (token-accurate when a model tokenizer is available),
///    with a capacity range for better packing and best-effort overlap.
/// 3. Each chunk's byte span is taken directly from the splitter; its
///    `heading_path` is inherited from the block containing the chunk's start.
fn chunk_prose(
    document_id: &str,
    full_text: &str,
    blocks: &[Block],
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    let target = config.resolved_target_tokens();
    let overlap = config.resolved_overlap_tokens();

    if blocks.is_empty() {
        // If no blocks but there is text, make a single chunk of the whole text.
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

    if full_text.is_empty() {
        return Ok(vec![]);
    }

    // Capacity range enables better packing: aim between 3/4 target and target.
    // Use inclusive range so the effective max is exactly `target` (exclusive `..target` gives max = target-1).
    let cap_start = target * 3 / 4;
    let cap = cap_start..=target;

    let ts_sizer = TsSizer(sizer);
    let mut cfg = text_splitter::ChunkConfig::new(cap).with_sizer(ts_sizer);
    // Overlap is best-effort; only apply when valid (0 < overlap < cap.start).
    if overlap > 0 && overlap < cap_start {
        match cfg.with_overlap(overlap) {
            Ok(c) => cfg = c,
            Err(_) => {
                // Rebuild without overlap (with_overlap consumed cfg).
                let ts_sizer = TsSizer(sizer);
                cfg = text_splitter::ChunkConfig::new(cap_start..=target).with_sizer(ts_sizer);
            }
        }
    }

    let splitter = text_splitter::MarkdownSplitter::new(cfg);

    let mut chunks = Vec::new();
    for (byte_off, chunk) in splitter.chunk_indices(full_text) {
        let start = byte_off;
        let end = byte_off + chunk.len();
        let span = Span::new(start, end);

        // Inherit heading_path from the block whose span contains the chunk start,
        // else the last block starting at or before it.
        let heading_path = blocks
            .iter()
            .rev()
            .find(|b| b.span.start <= start && start < b.span.end)
            .or_else(|| blocks.iter().rev().find(|b| b.span.start <= start))
            .map(|b| b.heading_path.clone())
            .unwrap_or_default();

        let id = chunk_id(document_id, chunk, start, end);
        chunks.push(ChunkOutput {
            id,
            text: chunk.to_string(),
            span,
            heading_path,
        });
    }

    Ok(chunks)
}

// ---------------------------------------------------------------------------
// Code chunker
// ---------------------------------------------------------------------------

/// Code chunker: line-based chunking targeting ~60 lines per chunk.
///
/// Splits at blank lines or after `target_tokens` (interpreted as chars) of content.
fn chunk_code(
    document_id: &str,
    full_text: &str,
    blocks: &[Block],
    config: &ChunkerConfig,
) -> Result<Vec<ChunkOutput>, Error> {
    let target = config.resolved_target_tokens();

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

    /// Word-count sizer for tests — no model download required.
    struct WordSizer;
    impl ChunkSizer for WordSizer {
        fn size(&self, t: &str) -> usize {
            t.split_whitespace().count()
        }
    }

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
        assert_eq!(cfg.resolved_target_tokens(), 512);
        assert_eq!(cfg.resolved_overlap_tokens(), 64);
    }

    #[test]
    fn chunker_config_code_defaults() {
        let cfg = ChunkerConfig::code();
        assert_eq!(cfg.preset, "code");
        assert_eq!(cfg.resolved_target_tokens(), 3000);
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
        let result = chunk_document(&doc_id, "", &[], &cfg, &CharSizer).unwrap();
        assert!(result.is_empty(), "empty doc should produce no chunks");
    }

    #[test]
    fn prose_chunk_single_block() {
        let full_text = "Hello, this is a paragraph.";
        let blocks = vec![make_block(0, BlockKind::Paragraph, full_text, 0)];
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg, &CharSizer).unwrap();
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

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg, &CharSizer).unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(
                chunk.span.end <= full_text.len(),
                "span end must be within text"
            );
            assert!(chunk.span.start <= chunk.span.end, "span start <= end");
            let span_text = &full_text[chunk.span.start..chunk.span.end];
            assert!(!span_text.is_empty(), "span must reference non-empty text");
        }
    }

    #[test]
    fn prose_spans_round_trip() {
        let full_text =
            "# Heading One\n\nParagraph one with some words.\n\n## Heading Two\n\nParagraph two here.";
        let blocks = vec![
            make_heading_block(0, "Heading One", 0, vec!["Heading One".to_string()]),
            make_para_block(
                1,
                "Paragraph one with some words.",
                15,
                vec!["Heading One".to_string()],
            ),
        ];
        let doc_id = document_id("file:///rt.md", "abc");
        let cfg = ChunkerConfig::prose();
        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg, &WordSizer).unwrap();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert_eq!(
                &full_text[c.span.start..c.span.end],
                c.text,
                "span byte slice must equal chunk text"
            );
        }
    }

    #[test]
    fn prose_respects_token_target_with_word_sizer() {
        // Build a long multi-paragraph markdown doc.
        let para = "word ".repeat(40); // 40 "words" per paragraph
        let mut full_text = String::new();
        for i in 0..10 {
            full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
        }
        let blocks = vec![make_para_block(0, &full_text, 0, vec![])];
        let doc_id = document_id("file:///long.md", "abc");
        // Generous target so the assertion is robust.
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(60),
            overlap_tokens: Some(8),
        };
        let chunks = chunk_document(&doc_id, &full_text, &blocks, &cfg, &WordSizer).unwrap();
        assert!(
            chunks.len() >= 2,
            "long doc should produce multiple chunks, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                WordSizer.size(&c.text) <= 60,
                "chunk should respect token target: {} words",
                WordSizer.size(&c.text)
            );
        }
    }

    #[test]
    fn prose_chunks_in_document_order() {
        let para = "word ".repeat(40);
        let mut full_text = String::new();
        for i in 0..6 {
            full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
        }
        let blocks = vec![make_para_block(0, &full_text, 0, vec![])];
        let doc_id = document_id("file:///order.md", "abc");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(60),
            overlap_tokens: Some(0),
        };
        let chunks = chunk_document(&doc_id, &full_text, &blocks, &cfg, &WordSizer).unwrap();
        for w in chunks.windows(2) {
            assert!(
                w[0].span.start <= w[1].span.start,
                "chunks must be in document order"
            );
        }
    }

    #[test]
    fn prose_char_sizer_fallback_produces_chunks() {
        let full_text = "# Title\n\nSome prose content here for the char sizer fallback path.";
        let blocks = vec![make_para_block(0, full_text, 0, vec![])];
        let doc_id = document_id("file:///char.md", "abc");
        let cfg = ChunkerConfig::prose();
        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg, &CharSizer).unwrap();
        assert!(
            !chunks.is_empty(),
            "char sizer fallback should produce chunks"
        );
    }

    #[test]
    fn prose_chunk_large_text_splits_into_multiple_chunks() {
        let para = "word ".repeat(100);
        let mut full_text = String::new();
        for i in 0..8 {
            full_text.push_str(&format!("## Para {i}\n\n{para}\n\n"));
        }
        let blocks = vec![make_para_block(0, &full_text, 0, vec![])];
        let doc_id = document_id("file:///large.md", "hash");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(80),
            overlap_tokens: Some(0),
        };
        let chunks = chunk_document(&doc_id, &full_text, &blocks, &cfg, &WordSizer).unwrap();
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

        let chunks1 = chunk_document(&doc_id, full_text, &blocks, &cfg, &CharSizer).unwrap();
        let chunks2 = chunk_document(&doc_id, full_text, &blocks, &cfg, &CharSizer).unwrap();

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
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(8),
            overlap_tokens: Some(0),
        };

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg, &WordSizer).unwrap();
        assert!(!chunks.is_empty());
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
            target_tokens: None,
            overlap_tokens: None,
        };

        let result = chunk_document(&doc_id, full_text, &blocks, &cfg, &CharSizer);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "invalid_request");
    }

    #[test]
    fn prose_multibyte_utf8_no_panic() {
        let text = "こんにちは world — это тест";
        let doc_id = "doc-multibyte";
        let blocks = vec![make_block(0, BlockKind::Paragraph, text, 0)];
        let result = chunk_document(doc_id, text, &blocks, &ChunkerConfig::prose(), &CharSizer);
        assert!(
            result.is_ok(),
            "chunking multi-byte text should not panic: {:?}",
            result.err()
        );
    }

    #[test]
    fn prose_overlap_skipped_when_at_or_above_cap_start() {
        // When overlap >= cap_start (target * 3/4), the guard disables overlap
        // rather than passing an invalid value to text-splitter. The splitter must
        // still produce valid, ordered chunks.
        let para = "word ".repeat(50);
        let mut full_text = String::new();
        for i in 0..4 {
            full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
        }
        let blocks = vec![make_para_block(0, &full_text, 0, vec![])];
        let doc_id = document_id("file:///overlap_guard.md", "abc");
        // overlap (60) == cap_start (target*3/4 = 60 when target=80): guard blocks it.
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(80),
            overlap_tokens: Some(60), // == cap_start → guard prevents passing to library
        };
        let chunks = chunk_document(&doc_id, &full_text, &blocks, &cfg, &WordSizer).unwrap();
        assert!(
            !chunks.is_empty(),
            "should produce chunks even with skipped overlap"
        );
        // Chunks must be in document order.
        for w in chunks.windows(2) {
            assert!(
                w[0].span.start <= w[1].span.start,
                "chunks must be in order"
            );
        }
    }

    #[test]
    fn prose_oversized_single_atomic_unit_no_panic() {
        // A single "word" (no whitespace) longer than the target must be emitted as
        // an over-max chunk without panicking, and its span must round-trip.
        let long_word = "a".repeat(2000); // 2000 chars, way above default char target
        let full_text = format!("# Title\n\n{long_word}");
        let blocks = vec![
            make_heading_block(0, "Title", 0, vec!["Title".to_string()]),
            make_para_block(1, &long_word, 9, vec!["Title".to_string()]),
        ];
        let doc_id = document_id("file:///oversized.md", "abc");
        // Small target ensures the long word exceeds it.
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(20),
            overlap_tokens: Some(0),
        };
        let result = chunk_document(&doc_id, &full_text, &blocks, &cfg, &CharSizer);
        assert!(
            result.is_ok(),
            "oversized atomic unit should not panic: {:?}",
            result.err()
        );
        let chunks = result.unwrap();
        assert!(!chunks.is_empty());
        // Span round-trip.
        for c in &chunks {
            assert_eq!(
                &full_text[c.span.start..c.span.end],
                c.text,
                "oversized chunk span must round-trip"
            );
        }
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

        let chunks = chunk_document(&doc_id, full_text, &blocks, &cfg, &CharSizer).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, full_text);
    }

    #[test]
    fn code_chunk_large_splits() {
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

        let chunks = chunk_document(&doc_id, &full_text, &blocks, &cfg, &CharSizer).unwrap();
        assert!(
            chunks.len() >= 2,
            "large code file should produce multiple chunks"
        );
    }

    #[test]
    fn chunk_document_multibyte_code_preset_does_not_panic() {
        let unit = "日本語テキスト: これはテストです。 ";
        let text = unit.repeat(200); // large enough to trigger splits
        let doc_id = "doc-multibyte-code";
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
        let result = chunk_document(doc_id, &text, &blocks, &ChunkerConfig::code(), &CharSizer);
        assert!(
            result.is_ok(),
            "code chunking multi-byte text should not panic: {:?}",
            result.err()
        );
    }
}
