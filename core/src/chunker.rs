//! Chunking logic for the ingestion pipeline.
//!
//! Implements the per-source-kind presets from specs/04-search-pipeline.md §3:
//! - `prose` (default): Markdown-structure-aware split (via `text-splitter`),
//!   token-accurate to the model tokenizer, target ~512 tokens with ~64 overlap.
//!   The splitter now receives REAL Markdown (headings, fences, bullets preserved).
//! - `code` (interim): simple line-based text packer; the future AST chunker
//!   (text-splitter::CodeSplitter) will supersede this. See specs/04-search-pipeline.md §2.
//! - `messages` (reserved): not implemented in v1
//!
//! Heading-path attribution uses `heading_index::build_heading_index` over the
//! real Markdown string, replacing the old Block-based sidecar.

use std::sync::Arc;

use crate::ids::{chunk_id, ContentId};
use crate::types::Span;
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
/// Markdown string. `heading_path` is derived from the Markdown heading structure.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkOutput {
    /// Content-addressed chunk ID: `blake3(document_id || text || span)`.
    pub id: ContentId,
    /// Chunk text (a slice of the Markdown string).
    pub text: String,
    /// Byte range in the Markdown string.
    pub span: Span,
    /// Heading path at the chunk's start offset.
    pub heading_path: Vec<String>,
    /// Block sequence number this chunk came from (0 when not block-aware).
    pub block_seq: u32,
    /// Position of this chunk within the block (0-indexed).
    pub seq_in_block: u32,
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
    /// it is still interpreted as a character budget (interim line packer).
    pub target_tokens: Option<usize>,
    /// Overlap in tokens. `None` = preset default.
    pub overlap_tokens: Option<usize>,
}

impl ChunkerConfig {
    /// Create a config for the `prose` preset with the spec defaults.
    ///
    /// Default target ≈ 256 tokens, overlap ≈ 0 tokens. These match the
    /// contextual training regime of `pplx-embed-context-v1` (256-token chunks,
    /// no intra-document overlap — late chunking supplies cross-chunk context).
    /// See specs/04-search-pipeline.md §3.
    pub fn prose() -> Self {
        Self {
            preset: "prose".to_string(),
            target_tokens: Some(256),
            overlap_tokens: Some(0),
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
        self.target_tokens.unwrap_or(256)
    }

    /// Resolved overlap tokens (uses preset default if not overridden).
    pub fn resolved_overlap_tokens(&self) -> usize {
        self.overlap_tokens.unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Block-aware chunk function
// ---------------------------------------------------------------------------

/// Chunk a sequence of typed [`Block`]s into `ChunkOutput` records.
///
/// Dispatches each block by kind:
/// - `Heading`, `Paragraph`, `Quote`, `List`, `Message`, `Segment` → prose chunker.
/// - `Code`, `Table` → code chunker.
/// - `Reference`, `Attachment`, `Frontmatter`, `Image` → single chunk per block.
///
/// For each sub-chunk within a block:
/// - `block_seq` is set to `block.seq`.
/// - `seq_in_block` is set to the chunk's index within that block.
/// - `heading_path` is derived from `heading_path_from_blocks`.
///
/// Blocks with empty text are skipped.
pub fn chunk_blocks(
    resource_id: &str,
    blocks: &[crate::block::Block],
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    use crate::block::BlockKind;
    use crate::markdown_blocks::heading_path_from_blocks;

    let mut out: Vec<ChunkOutput> = Vec::new();

    for block in blocks {
        if block.text.is_empty() {
            continue;
        }

        let heading_path = heading_path_from_blocks(blocks, block.seq);

        let sub_chunks: Vec<ChunkOutput> = match &block.kind {
            // Prose-style blocks
            BlockKind::Heading { .. }
            | BlockKind::Paragraph
            | BlockKind::Quote
            | BlockKind::List { .. }
            | BlockKind::Message { .. }
            | BlockKind::Segment { .. } => {
                chunk_prose(resource_id, &block.text, config, sizer)?
            }
            // Code/table blocks
            BlockKind::Code { .. } | BlockKind::Table { .. } => {
                chunk_code(resource_id, &block.text, config)?
            }
            // Single-block pass-through
            BlockKind::Reference { .. }
            | BlockKind::Attachment { .. }
            | BlockKind::Frontmatter { .. }
            | BlockKind::Image { .. } => {
                let text = &block.text;
                let id = chunk_id(resource_id, text, 0, text.len());
                vec![ChunkOutput {
                    id,
                    text: text.clone(),
                    span: crate::types::Span::new(0, text.len()),
                    heading_path: heading_path.clone(),
                    block_seq: block.seq,
                    seq_in_block: 0,
                }]
            }
        };

        for (i, mut c) in sub_chunks.into_iter().enumerate() {
            c.block_seq = block.seq;
            c.seq_in_block = i as u32;
            if c.heading_path.is_empty() {
                c.heading_path = heading_path.clone();
            }
            out.push(c);
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Main chunk function
// ---------------------------------------------------------------------------

/// Chunk a Markdown document string into `ChunkOutput` records.
///
/// The `document_id` is used to derive chunk IDs (content-addressed).
/// `markdown` is the normalized Markdown string returned by the extractor.
///
/// Respects preset and per-document configuration. The `sizer` measures chunk
/// sizes for the `prose` preset (the `code` preset uses its own char budget).
///
/// # Errors
/// - Returns `Error::InvalidRequest` if the preset is unknown or reserved.
pub fn chunk_document(
    document_id: &str,
    markdown: &str,
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    match config.preset.as_str() {
        "prose" => chunk_prose(document_id, markdown, config, sizer),
        "code" => chunk_code(document_id, markdown, config),
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
/// Feeds REAL Markdown (with `#`, fences, bullets) to `MarkdownSplitter`,
/// fixing the latent smell where stripped text was passed before.
/// Heading-path attribution uses `heading_index::build_heading_index` over the
/// same Markdown string — no Block sidecar needed.
fn chunk_prose(
    document_id: &str,
    markdown: &str,
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    if markdown.is_empty() {
        return Ok(vec![]);
    }

    let target = config.resolved_target_tokens();

    // Layer D: backstop for structureless files misclassified as prose.
    // If the longest line exceeds 8× the char target, delegate to chunk_code.
    {
        let max_line_len = markdown
            .lines()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        tracing::debug!(
            max_line_len,
            threshold = 8 * target,
            "chunk_prose backstop probe"
        );
        if max_line_len > 8 * target {
            tracing::debug!(
                max_line_len,
                "chunk_prose backstop: delegating to chunk_code"
            );
            return chunk_code(document_id, markdown, config);
        }
    }

    let overlap = config.resolved_overlap_tokens();

    let heading_idx = crate::heading_index::build_heading_index(markdown);

    // Capacity range enables better packing: aim between 3/4 target and target.
    let cap_start = target * 3 / 4;
    let cap = cap_start..=target;

    let ts_sizer = TsSizer(sizer);
    let mut cfg = text_splitter::ChunkConfig::new(cap).with_sizer(ts_sizer);
    // Overlap is best-effort; only apply when valid (0 < overlap < cap_start).
    if overlap > 0 && overlap < cap_start {
        match cfg.with_overlap(overlap) {
            Ok(c) => cfg = c,
            Err(_) => {
                let ts_sizer = TsSizer(sizer);
                cfg = text_splitter::ChunkConfig::new(cap_start..=target).with_sizer(ts_sizer);
            }
        }
    }

    let splitter = text_splitter::MarkdownSplitter::new(cfg);

    let mut chunks = Vec::new();
    for (byte_off, chunk) in splitter.chunk_indices(markdown) {
        let start = byte_off;
        let end = byte_off + chunk.len();
        let span = Span::new(start, end);
        let heading_path = crate::heading_index::heading_path_at(&heading_idx, start);
        let id = chunk_id(document_id, chunk, start, end);
        chunks.push(ChunkOutput {
            id,
            text: chunk.to_string(),
            span,
            heading_path,
            block_seq: 0,
            seq_in_block: 0,
        });
    }

    Ok(chunks)
}

// ---------------------------------------------------------------------------
// Code chunker (interim)
// ---------------------------------------------------------------------------

/// Code chunker: interim line-based text packer over the Markdown string.
///
/// NOTE: This is a temporary downgrade from the old block-driven code chunker.
/// It will be superseded by `text-splitter::CodeSplitter` (tree-sitter) when
/// code sources become a focus. See specs/04-search-pipeline.md §2.
fn chunk_code(
    document_id: &str,
    markdown: &str,
    config: &ChunkerConfig,
) -> Result<Vec<ChunkOutput>, Error> {
    if markdown.is_empty() {
        return Ok(vec![]);
    }

    let target = config.resolved_target_tokens(); // used as char budget
    let mut chunks = Vec::new();
    let mut current_start = 0usize;
    let mut current_end = 0usize;

    for (line_start, line) in line_offsets(markdown) {
        let line_end = line_start + line.len();

        // Hard-split overlong lines at char boundaries.
        if line.chars().count() > target {
            // Flush any pending content first.
            if current_end > current_start {
                let cs = floor_char_boundary(markdown, current_start);
                let ce = floor_char_boundary(markdown, current_end);
                if cs < ce {
                    let chunk_text = &markdown[cs..ce];
                    let id = chunk_id(document_id, chunk_text, cs, ce);
                    chunks.push(ChunkOutput {
                        id,
                        text: chunk_text.to_string(),
                        span: Span::new(cs, ce),
                        heading_path: vec![],
                        block_seq: 0,
                        seq_in_block: 0,
                    });
                }
            }

            // Split the overlong line into target-sized char pieces.
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
                let piece_end = (pos + byte_len).min(line_end);
                if pos < piece_end {
                    let chunk_text = &markdown[pos..piece_end];
                    let id = chunk_id(document_id, chunk_text, pos, piece_end);
                    chunks.push(ChunkOutput {
                        id,
                        text: chunk_text.to_string(),
                        span: Span::new(pos, piece_end),
                        heading_path: vec![],
                        block_seq: 0,
                        seq_in_block: 0,
                    });
                }
                pos = piece_end;
            }
            current_start = line_end;
            current_end = line_end;
            continue;
        }

        let current_size = current_end.saturating_sub(current_start);

        if current_size > 0 && current_size + (line_end - line_start) > target {
            let cs = floor_char_boundary(markdown, current_start);
            let ce = floor_char_boundary(markdown, current_end);
            if cs < ce {
                let chunk_text = &markdown[cs..ce];
                let id = chunk_id(document_id, chunk_text, cs, ce);
                chunks.push(ChunkOutput {
                    id,
                    text: chunk_text.to_string(),
                    span: Span::new(cs, ce),
                    heading_path: vec![],
                    block_seq: 0,
                    seq_in_block: 0,
                });
            }
            current_start = line_start;
        }

        if current_size == 0 {
            current_start = line_start;
        }
        current_end = line_end;
    }

    // Flush remaining content.
    if current_end > current_start {
        let cs = floor_char_boundary(markdown, current_start);
        let ce = floor_char_boundary(markdown, current_end);
        if cs < ce {
            let chunk_text = &markdown[cs..ce];
            let id = chunk_id(document_id, chunk_text, cs, ce);
            chunks.push(ChunkOutput {
                id,
                text: chunk_text.to_string(),
                span: Span::new(cs, ce),
                heading_path: vec![],
                block_seq: 0,
                seq_in_block: 0,
            });
        }
    }

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

/// Classify a file as `"code"` or `"prose"` based on MIME type and filename.
///
/// Logic: check mime first, then filename basename for lockfiles, then extension.
/// Falls back to `"prose"` if nothing matches.
pub fn preset_for(filename: Option<&str>, mime: Option<&str>) -> &'static str {
    const CODE_EXTS: &[&str] = &[
        "rs", "py", "js", "mjs", "ts", "tsx", "json", "yaml", "yml", "toml", "lock", "c", "h",
        "cpp", "hpp", "go", "java", "rb", "php", "sh", "css", "scss", "sql", "csv", "xml", "ini",
        "cfg", "xlsx", "xls",
    ];
    const PROSE_EXTS: &[&str] = &["md", "markdown", "html", "htm", "pdf", "txt", "text"];
    const LOCKFILE_BASENAMES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "poetry.lock",
        "Gemfile.lock",
    ];

    // Check MIME first.
    if let Some(m) = mime {
        if m == "application/json" || m.starts_with("text/x-") {
            return "code";
        }
        if m == "text/plain" {
            return "prose";
        }
    }

    // Check lockfile basenames (case-sensitive).
    if let Some(name) = filename {
        let basename = std::path::Path::new(name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(name);
        if LOCKFILE_BASENAMES.contains(&basename) {
            return "code";
        }

        // Check extension (case-insensitive).
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase());
        if let Some(ext) = ext {
            if CODE_EXTS.contains(&ext.as_str()) {
                return "code";
            }
            if PROSE_EXTS.contains(&ext.as_str()) {
                return "prose";
            }
        }
    }

    // Default: prose (safe fallback)
    "prose"
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::document_id;

    /// Word-count sizer for tests — no model download required.
    struct WordSizer;
    impl ChunkSizer for WordSizer {
        fn size(&self, t: &str) -> usize {
            t.split_whitespace().count()
        }
    }

    // ---------------------------------------------------------------------------
    // ChunkerConfig tests
    // ---------------------------------------------------------------------------

    #[test]
    fn chunker_config_prose_defaults() {
        let cfg = ChunkerConfig::prose();
        assert_eq!(cfg.preset, "prose");
        assert_eq!(cfg.resolved_target_tokens(), 256);
        assert_eq!(cfg.resolved_overlap_tokens(), 0);
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
        let result = chunk_document(&doc_id, "", &cfg, &CharSizer).unwrap();
        assert!(result.is_empty(), "empty doc should produce no chunks");
    }

    #[test]
    fn prose_chunk_single_paragraph() {
        let full_text = "Hello, this is a paragraph.";
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_document(&doc_id, full_text, &cfg, &CharSizer).unwrap();
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        assert!(
            chunks.iter().any(|c| c.text.contains("Hello")),
            "chunk should contain the paragraph text"
        );
    }

    #[test]
    fn prose_chunk_span_references_markdown() {
        let full_text = "# Introduction\n\nThis is the intro paragraph.";
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_document(&doc_id, full_text, &cfg, &CharSizer).unwrap();
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
        let doc_id = document_id("file:///rt.md", "abc");
        let cfg = ChunkerConfig::prose();
        let chunks = chunk_document(&doc_id, full_text, &cfg, &WordSizer).unwrap();
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
        let para = "word ".repeat(40);
        let mut full_text = String::new();
        for i in 0..10 {
            full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
        }
        let doc_id = document_id("file:///long.md", "abc");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(60),
            overlap_tokens: Some(8),
        };
        let chunks = chunk_document(&doc_id, &full_text, &cfg, &WordSizer).unwrap();
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
        let doc_id = document_id("file:///order.md", "abc");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(60),
            overlap_tokens: Some(0),
        };
        let chunks = chunk_document(&doc_id, &full_text, &cfg, &WordSizer).unwrap();
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
        let doc_id = document_id("file:///char.md", "abc");
        let cfg = ChunkerConfig::prose();
        let chunks = chunk_document(&doc_id, full_text, &cfg, &CharSizer).unwrap();
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
        let doc_id = document_id("file:///large.md", "hash");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(80),
            overlap_tokens: Some(0),
        };
        let chunks = chunk_document(&doc_id, &full_text, &cfg, &WordSizer).unwrap();
        assert!(
            chunks.len() >= 2,
            "large document should produce multiple chunks, got {}",
            chunks.len()
        );
    }

    #[test]
    fn prose_chunk_ids_are_content_addressed() {
        let full_text = "Hello world this is content.";
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks1 = chunk_document(&doc_id, full_text, &cfg, &CharSizer).unwrap();
        let chunks2 = chunk_document(&doc_id, full_text, &cfg, &CharSizer).unwrap();

        assert_eq!(chunks1.len(), chunks2.len());
        for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(c1.id, c2.id, "chunk IDs must be deterministic");
        }
    }

    #[test]
    fn prose_chunk_heading_path_inherited_from_markdown() {
        // The splitter now sees real Markdown — heading_path is derived from the
        // Markdown heading structure, not from a Block sidecar.
        let full_text = "# API\n\nAPI documentation.\n\n# Auth\n\nAuth documentation.";
        let doc_id = document_id("file:///api.md", "abc");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(8),
            overlap_tokens: Some(0),
        };

        let chunks = chunk_document(&doc_id, full_text, &cfg, &WordSizer).unwrap();
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
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: None,
            overlap_tokens: None,
        };

        let result = chunk_document(&doc_id, full_text, &cfg, &CharSizer);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "invalid_request");
    }

    #[test]
    fn prose_multibyte_utf8_no_panic() {
        let text = "こんにちは world — это тест";
        let doc_id = "doc-multibyte";
        let result = chunk_document(doc_id, text, &ChunkerConfig::prose(), &CharSizer);
        assert!(
            result.is_ok(),
            "chunking multi-byte text should not panic: {:?}",
            result.err()
        );
    }

    #[test]
    fn prose_overlap_skipped_when_at_or_above_cap_start() {
        let para = "word ".repeat(50);
        let mut full_text = String::new();
        for i in 0..4 {
            full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
        }
        let doc_id = document_id("file:///overlap_guard.md", "abc");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(80),
            overlap_tokens: Some(60),
        };
        let chunks = chunk_document(&doc_id, &full_text, &cfg, &WordSizer).unwrap();
        assert!(
            !chunks.is_empty(),
            "should produce chunks even with skipped overlap"
        );
        for w in chunks.windows(2) {
            assert!(
                w[0].span.start <= w[1].span.start,
                "chunks must be in order"
            );
        }
    }

    #[test]
    fn prose_oversized_single_atomic_unit_no_panic() {
        let long_word = "a".repeat(2000);
        let full_text = format!("# Title\n\n{long_word}");
        let doc_id = document_id("file:///oversized.md", "abc");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(20),
            overlap_tokens: Some(0),
        };
        let result = chunk_document(&doc_id, &full_text, &cfg, &CharSizer);
        assert!(
            result.is_ok(),
            "oversized atomic unit should not panic: {:?}",
            result.err()
        );
        let chunks = result.unwrap();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert_eq!(
                &full_text[c.span.start..c.span.end],
                c.text,
                "oversized chunk span must round-trip"
            );
        }
    }

    #[test]
    fn prose_splitter_sees_real_markdown_structure() {
        // Verify the splitter actually receives real Markdown (the `#` heading marker
        // must be present in chunk text so MarkdownSplitter can split on structure).
        let md =
            "# Section One\n\nContent of section one.\n\n# Section Two\n\nContent of section two.";
        let doc_id = document_id("file:///structure.md", "abc");
        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(8),
            overlap_tokens: Some(0),
        };
        let chunks = chunk_document(&doc_id, md, &cfg, &WordSizer).unwrap();
        // At least one chunk should contain the `#` character (real Markdown, not stripped).
        assert!(
            chunks.iter().any(|c| c.text.contains('#')),
            "at least one chunk should contain the # heading marker; got: {:?}",
            chunks.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    // ---------------------------------------------------------------------------
    // Code chunker tests (interim line packer)
    // ---------------------------------------------------------------------------

    #[test]
    fn code_chunk_empty_returns_empty() {
        let doc_id = document_id("file:///lib.rs", "abc");
        let cfg = ChunkerConfig::code();
        let chunks = chunk_document(&doc_id, "", &cfg, &CharSizer).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn code_chunk_single_block() {
        let full_text = "fn hello() {\n    println!(\"hi\");\n}";
        let doc_id = document_id("file:///lib.rs", "abc");
        let cfg = ChunkerConfig::code();

        let chunks = chunk_document(&doc_id, full_text, &cfg, &CharSizer).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, full_text);
    }

    #[test]
    fn code_chunk_large_splits() {
        let line = "let x = some_function_with_long_name(arg1, arg2, arg3);\n";
        let full_text = line.repeat(100); // ~5600 chars
        let doc_id = document_id("file:///lib.rs", "hash");
        let cfg = ChunkerConfig::code();

        let chunks = chunk_document(&doc_id, &full_text, &cfg, &CharSizer).unwrap();
        assert!(
            chunks.len() >= 2,
            "large code file should produce multiple chunks"
        );
    }

    #[test]
    fn code_chunk_spans_round_trip() {
        let line = "let x = 1;\n";
        let full_text = line.repeat(200);
        let doc_id = document_id("file:///lib.rs", "hash");
        let cfg = ChunkerConfig::code();
        let chunks = chunk_document(&doc_id, &full_text, &cfg, &CharSizer).unwrap();
        for c in &chunks {
            assert_eq!(
                &full_text[c.span.start..c.span.end],
                c.text,
                "code chunk span must round-trip"
            );
        }
    }

    #[test]
    fn chunk_document_multibyte_code_preset_does_not_panic() {
        let unit = "日本語テキスト: これはテストです。 ";
        let text = unit.repeat(200);
        let doc_id = "doc-multibyte-code";
        let result = chunk_document(doc_id, &text, &ChunkerConfig::code(), &CharSizer);
        assert!(
            result.is_ok(),
            "code chunking multi-byte text should not panic: {:?}",
            result.err()
        );
    }

    // ---------------------------------------------------------------------------
    // Layer A: preset_for routing tests
    // ---------------------------------------------------------------------------

    #[test]
    fn preset_for_routes_code_extensions() {
        assert_eq!(preset_for(Some("lib.rs"), None), "code");
        assert_eq!(preset_for(Some("data.json"), None), "code");
        assert_eq!(preset_for(Some("config.toml"), None), "code");
        assert_eq!(preset_for(Some("Cargo.lock"), None), "code");
        assert_eq!(preset_for(None, Some("application/json")), "code");
        assert_eq!(preset_for(None, Some("text/x-rust")), "code");
    }

    #[test]
    fn preset_for_routes_prose() {
        assert_eq!(preset_for(Some("README.md"), None), "prose");
        assert_eq!(preset_for(Some("notes.txt"), None), "prose");
        assert_eq!(preset_for(Some("page.html"), None), "prose");
        assert_eq!(preset_for(Some("doc.pdf"), None), "prose");
        assert_eq!(preset_for(None, Some("text/plain")), "prose");
    }

    // ---------------------------------------------------------------------------
    // Layer D: structureless and overlong line tests
    // ---------------------------------------------------------------------------

    #[test]
    fn code_hard_splits_overlong_line() {
        // A single line of ~100k chars should produce multiple bounded chunks.
        let long_line = "x".repeat(100_000);
        let doc_id = "doc-overlong";
        let cfg = ChunkerConfig::code(); // target = 3000 chars
        let chunks = chunk_document(doc_id, &long_line, &cfg, &CharSizer).unwrap();
        assert!(
            chunks.len() >= 2,
            "overlong line should produce multiple chunks, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(
                c.text.chars().count() <= 3000,
                "each chunk must be within target: {} chars",
                c.text.chars().count()
            );
        }
    }

    #[test]
    fn prose_structureless_long_line_falls_back_to_line_packer() {
        // A single very long line (no newlines, no headings) given to the prose
        // chunker should not hang — it must fall back to the code packer.
        let long_line = "word ".repeat(10_000); // ~50k chars, no newlines
        let doc_id = "doc-structureless";
        let cfg = ChunkerConfig::prose(); // target = 256 chars (prose default)
        let chunks = chunk_document(doc_id, &long_line, &cfg, &CharSizer).unwrap();
        assert!(
            !chunks.is_empty(),
            "structureless prose should produce chunks"
        );
        // Should not take forever — the backstop kicks in.
        // (The test completing at all is the key assertion.)
    }

    // ---------------------------------------------------------------------------
    // Regression tests: hang-fix for code/JSON/lockfiles (only-index-supported-files)
    // ---------------------------------------------------------------------------

    /// Regression: minified JSON (one very long line) must not hang and must produce
    /// bounded chunks. Before the fix, `chunk_prose` was called on structureless JSON,
    /// causing super-linear cost and a multi-minute hang.
    #[test]
    fn regression_minified_json_does_not_hang() {
        let unit = r#"{"key":"value","#;
        // 100_000 chars ≈ 6250 repetitions of the 16-char unit
        let reps = 100_000 / unit.len();
        let content = unit.repeat(reps);
        let doc_id = "doc-minified-json";
        let cfg = ChunkerConfig::code(); // target = 3000 chars
        let chunks = chunk_document(doc_id, &content, &cfg, &CharSizer).unwrap();
        // Must produce more than one chunk (content >> target).
        assert!(
            chunks.len() > 1,
            "minified JSON must split into multiple chunks, got {}",
            chunks.len()
        );
        // Every chunk must be within 2× the char target.
        let target = cfg.resolved_target_tokens();
        for c in &chunks {
            let char_count = c.text.chars().count();
            assert!(
                char_count <= 2 * target,
                "chunk exceeds 2× target ({} chars, target {})",
                char_count,
                target
            );
        }
    }

    /// Regression: a Rust source file must be routed to the code chunker, not prose.
    /// Before the fix, `preset_for` did not exist and all files defaulted to prose.
    #[test]
    fn regression_code_file_uses_line_chunker_not_prose() {
        assert_eq!(
            preset_for(Some("main.rs"), None),
            "code",
            "main.rs must route to the code chunker"
        );
    }

    /// Regression: a Markdown README must still use the prose chunker.
    #[test]
    fn regression_prose_file_uses_prose_chunker() {
        assert_eq!(
            preset_for(Some("README.md"), None),
            "prose",
            "README.md must route to the prose chunker"
        );
    }

    /// Regression: Cargo.lock (lockfile, no recognized extension) must route to code.
    /// Before the fix, Cargo.lock would fall through to prose and hang on its
    /// long structureless sections.
    #[test]
    fn regression_cargo_lock_uses_line_chunker() {
        assert_eq!(
            preset_for(Some("Cargo.lock"), None),
            "code",
            "Cargo.lock must route to the code chunker"
        );
    }

    #[test]
    fn preset_for_spreadsheet_exts_is_code() {
        assert_eq!(preset_for(Some("sheet.xlsx"), None), "code");
        assert_eq!(preset_for(Some("sheet.xls"), None), "code");
        // Case-insensitive
        assert_eq!(preset_for(Some("SHEET.XLSX"), None), "code");
    }

    #[test]
    fn preset_for_docx_pptx_is_prose() {
        // DOCX and PPTX are prose documents, not tabular/code data.
        assert_eq!(preset_for(Some("report.docx"), None), "prose");
        assert_eq!(preset_for(Some("slides.pptx"), None), "prose");
    }

    #[test]
    fn preset_for_csv_is_code() {
        // Regression: CSV was already code, should still be.
        assert_eq!(preset_for(Some("data.csv"), None), "code");
    }
}
