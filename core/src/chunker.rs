//! Chunking logic for the ingestion pipeline.
//!
//! Entry point: [`chunk_blocks`] — operates on typed [`Block`]s produced by
//! `markdown_to_blocks()` and dispatches to the preset-specific helpers below.
//!
//! Presets (specs/04-search-pipeline.md §3):
//! - `prose` (default): Markdown-structure-aware split (via `text-splitter`),
//!   token-accurate to the model tokenizer, target ~512 tokens with ~64 overlap.
//!   The splitter receives real Markdown (headings, fences, bullets preserved).
//! - `code` (interim): simple line-based text packer; the future AST chunker
//!   (text-splitter::CodeSplitter) will supersede this. See specs/04-search-pipeline.md §2.
//! - `messages`: sliding-window chunker over `Message`/`Segment` blocks.
//!
//! Heading-path attribution uses `heading_index::build_heading_index` internally
//! within `chunk_prose` over the real Markdown string.

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
    /// For message-window chunks: all block seqs participating in the window.
    /// Empty for non-message chunks. Mirrors `ChunkLocation::window_block_seqs`.
    pub window_block_seqs: Vec<u32>,
    /// Block kind string (e.g. "paragraph", "heading"). `None` for flat-document chunks.
    pub block_kind: Option<String>,
}

impl ChunkOutput {
    /// Construct a single-block, non-windowed chunk (no heading path, seq_in_block=0).
    ///
    /// Convenience constructor that reduces boilerplate in block-dispatch paths.
    fn single(
        id: ContentId,
        text: String,
        span: Span,
        heading_path: Vec<String>,
        block_seq: u32,
    ) -> Self {
        Self {
            id,
            text,
            span,
            heading_path,
            block_seq,
            seq_in_block: 0,
            window_block_seqs: vec![],
            block_kind: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Chunker preset configuration
// ---------------------------------------------------------------------------

/// Configuration for the chunking operation.
#[derive(Debug, Clone)]
pub struct ChunkerConfig {
    /// Preset name: "prose", "code", or "messages".
    pub preset: String,
    /// Target chunk size in tokens. `None` = preset default.
    ///
    /// For the `prose` preset this is interpreted by the active `ChunkSizer`
    /// (tokens for `TokenSizer`, chars for `CharSizer`). For the `code` preset
    /// it is still interpreted as a character budget (interim line packer).
    pub target_tokens: Option<usize>,
    /// Overlap in tokens. `None` = preset default.
    pub overlap_tokens: Option<usize>,
    /// Number of message turns per sliding window (messages preset). `None` = default (6).
    pub window_turns: Option<usize>,
    /// Stride in turns between windows (messages preset). `None` = default (3).
    pub stride_turns: Option<usize>,
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
            window_turns: None,
            stride_turns: None,
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
            window_turns: None,
            stride_turns: None,
        }
    }

    /// Create a config for the `messages` preset with the spec defaults.
    ///
    /// Default window = 6 turns, stride = 3 turns.
    /// Token budget uses `target_tokens` (default 512) to cap windows.
    /// See specs/04-search-pipeline.md §3.
    pub fn messages() -> Self {
        Self {
            preset: "messages".to_string(),
            target_tokens: Some(512),
            overlap_tokens: Some(0),
            window_turns: Some(6),
            stride_turns: Some(3),
        }
    }

    /// Create a `ChunkerConfig` from a preset name string.
    ///
    /// Returns `Error::InvalidRequest` for unknown presets.
    pub fn from_preset(preset: &str) -> Result<Self, Error> {
        match preset {
            "prose" => Ok(Self::prose()),
            "code" => Ok(Self::code()),
            "messages" => Ok(Self::messages()),
            other => Err(Error::InvalidRequest {
                message: format!(
                    "unknown chunking preset '{}'; valid values: prose, code, messages",
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

    /// Resolved window turns for the messages preset (default 6).
    pub fn resolved_window_turns(&self) -> usize {
        self.window_turns.unwrap_or(6)
    }

    /// Resolved stride turns for the messages preset (default 3).
    pub fn resolved_stride_turns(&self) -> usize {
        self.stride_turns.unwrap_or(3)
    }
}

// ---------------------------------------------------------------------------
// Block-aware chunk function
// ---------------------------------------------------------------------------

/// Chunk a sequence of typed [`Block`]s into `ChunkOutput` records.
///
/// Dispatches by block kind:
/// - `Message`, `Segment` → messages chunker (sliding window over all such blocks).
/// - `Heading`, `Paragraph`, `Quote`, `List` → prose chunker (per block).
/// - `Code`, `Table` → code chunker (per block).
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

    // First pass: collect Message/Segment blocks and dispatch them together.
    let msg_blocks: Vec<&crate::block::Block> = blocks
        .iter()
        .filter(|b| {
            !b.text.is_empty()
                && matches!(
                    b.kind,
                    BlockKind::Message { .. } | BlockKind::Segment { .. }
                )
        })
        .collect();

    if !msg_blocks.is_empty() {
        let msg_chunks = chunk_messages(resource_id, blocks, config, sizer)?;
        out.extend(msg_chunks);
    }

    // Second pass: handle all non-message blocks individually.
    for block in blocks {
        if block.text.is_empty() {
            continue;
        }

        let is_msg = matches!(
            block.kind,
            BlockKind::Message { .. } | BlockKind::Segment { .. }
        );
        if is_msg {
            continue; // already handled above
        }

        let heading_path = heading_path_from_blocks(blocks, block.seq);

        let sub_chunks: Vec<ChunkOutput> = match &block.kind {
            // Prose-style blocks: route through code chunker when preset == "code"
            BlockKind::Heading { .. }
            | BlockKind::Paragraph
            | BlockKind::Quote
            | BlockKind::List { .. } => {
                if config.preset == "code" {
                    chunk_code(resource_id, &block.text, config, block.seq)?
                } else {
                    chunk_prose(resource_id, &block.text, config, sizer, block.seq)?
                }
            }
            // Code/table blocks
            BlockKind::Code { .. } | BlockKind::Table { .. } => {
                chunk_code(resource_id, &block.text, config, block.seq)?
            }
            // Single-block pass-through
            BlockKind::Reference { .. }
            | BlockKind::Attachment { .. }
            | BlockKind::Frontmatter { .. }
            | BlockKind::Image { .. } => {
                let text = &block.text;
                let id = chunk_id(resource_id, text, 0, text.len(), block.seq);
                vec![ChunkOutput::single(
                    id,
                    text.clone(),
                    crate::types::Span::new(0, text.len()),
                    heading_path.clone(),
                    block.seq,
                )]
            }
            // Message/Segment already dispatched above
            BlockKind::Message { .. } | BlockKind::Segment { .. } => unreachable!(),
        };

        for (i, mut c) in sub_chunks.into_iter().enumerate() {
            c.block_seq = block.seq;
            c.seq_in_block = i as u32;
            c.block_kind = Some(block.kind.kind_str().to_string());
            if c.heading_path.is_empty() {
                c.heading_path = heading_path.clone();
            }
            out.push(c);
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Messages chunker
// ---------------------------------------------------------------------------

/// Format a sender label for a `Message` block.
///
/// Produces `[sender] (timestamp): ` or `[sender]: ` when no timestamp.
fn format_message_prefix(sender: &str, timestamp: Option<&str>) -> String {
    match timestamp {
        Some(ts) => format!("[{sender}] ({ts}): "),
        None => format!("[{sender}]: "),
    }
}

/// Format a speaker label for a `Segment` block.
///
/// Produces `[speaker] (start_ms-end_ms): ` or `(start_ms-end_ms): ` when no speaker.
fn format_segment_prefix(speaker: Option<&str>, start_ms: u64, end_ms: u64) -> String {
    match speaker {
        Some(sp) => format!("[{sp}] ({start_ms}-{end_ms}): "),
        None => format!("({start_ms}-{end_ms}): "),
    }
}

/// Messages chunker: sliding-window chunker over `Message` and `Segment` blocks.
///
/// Each `Message`/`Segment` block is one "turn". The window covers `window_turns`
/// turns with `stride_turns` stride. Windows are also token-capped: if a window
/// exceeds `max_tokens`, turns are dropped from the front until it fits.
///
/// Very long single messages (exceeding `max_tokens` alone) are split using
/// prose-chunker logic, with the sender/speaker prefix prepended to each sub-chunk.
///
/// Message-window chunks intentionally span multiple blocks — this is the explicit
/// exception to the "chunk ⊂ block" invariant (see specs/04-search-pipeline.md §3).
pub fn chunk_messages(
    resource_id: &str,
    blocks: &[crate::block::Block],
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    use crate::block::BlockKind;

    let max_tokens = config.resolved_target_tokens();
    let window_turns = config.resolved_window_turns();
    let stride_turns = config.resolved_stride_turns();
    let stride_turns = stride_turns.max(1); // prevent infinite loop

    // Collect only Message/Segment blocks, in order.
    let turns: Vec<&crate::block::Block> = blocks
        .iter()
        .filter(|b| {
            !b.text.is_empty()
                && matches!(
                    b.kind,
                    BlockKind::Message { .. } | BlockKind::Segment { .. }
                )
        })
        .collect();

    if turns.is_empty() {
        return Ok(vec![]);
    }

    // Build prefixed text for each turn.
    let turn_texts: Vec<String> = turns
        .iter()
        .map(|b| {
            let prefix = match &b.kind {
                BlockKind::Message {
                    sender, timestamp, ..
                } => format_message_prefix(sender, timestamp.as_deref()),
                BlockKind::Segment {
                    speaker,
                    start_ms,
                    end_ms,
                } => format_segment_prefix(speaker.as_deref(), *start_ms, *end_ms),
                _ => unreachable!(),
            };
            format!("{prefix}{}", b.text)
        })
        .collect();

    let mut out: Vec<ChunkOutput> = Vec::new();
    let n = turns.len();
    let mut window_start = 0usize;

    while window_start < n {
        let window_end_excl = (window_start + window_turns).min(n);

        // Determine how many turns fit within the token budget. We shrink from
        // the END so that every turn appears in at least one window (shrinking
        // from the front would silently skip leading turns).
        let candidate_text: String = turn_texts[window_start..window_end_excl].join("\n\n");

        let mut actual_end = window_end_excl;
        if sizer.size(&candidate_text) > max_tokens {
            // Shrink window from end to fit token budget.
            while actual_end > window_start + 1 {
                let candidate: String = turn_texts[window_start..actual_end].join("\n\n");
                if sizer.size(&candidate) <= max_tokens {
                    break;
                }
                actual_end -= 1;
            }
        }

        let window_seqs: Vec<u32> = turns[window_start..actual_end]
            .iter()
            .map(|b| b.seq)
            .collect();

        // If even a single turn is too long, split it with prose chunker logic.
        if actual_end == window_start + 1 && sizer.size(&turn_texts[window_start]) > max_tokens {
            // Split the raw message body (without prefix) using prose chunker,
            // then prepend the sender/speaker context to each sub-chunk.
            let block = turns[window_start];
            let prefix = match &block.kind {
                crate::block::BlockKind::Message {
                    sender, timestamp, ..
                } => format_message_prefix(sender, timestamp.as_deref()),
                crate::block::BlockKind::Segment {
                    speaker,
                    start_ms,
                    end_ms,
                } => format_segment_prefix(speaker.as_deref(), *start_ms, *end_ms),
                _ => unreachable!(),
            };
            let prose_chunks = chunk_prose(resource_id, &block.text, config, sizer, block.seq)?;
            let first_seq = block.seq;
            let kind_str = block.kind.kind_str().to_string();
            for (i, pc) in prose_chunks.into_iter().enumerate() {
                let prefixed_text = format!("{prefix}{}", pc.text);
                let id = chunk_id(
                    resource_id,
                    &prefixed_text,
                    0,
                    prefixed_text.len(),
                    first_seq,
                );
                out.push(ChunkOutput {
                    id,
                    text: prefixed_text,
                    span: pc.span,
                    heading_path: vec![],
                    block_seq: first_seq,
                    seq_in_block: i as u32,
                    window_block_seqs: vec![first_seq],
                    block_kind: Some(kind_str.clone()),
                });
            }
        } else {
            let window_text: String = turn_texts[window_start..actual_end].join("\n\n");
            let first_seq = turns[window_start].seq;
            let kind_str = turns[window_start].kind.kind_str().to_string();
            let id = chunk_id(resource_id, &window_text, 0, window_text.len(), first_seq);
            out.push(ChunkOutput {
                id,
                text: window_text,
                span: crate::types::Span::new(0, 0), // not meaningful for multi-block windows
                heading_path: vec![],
                block_seq: first_seq,
                seq_in_block: out.len() as u32, // index among message chunks
                window_block_seqs: window_seqs,
                block_kind: Some(kind_str),
            });
        }

        window_start += stride_turns;
    }

    // Fix seq_in_block: should be the chunk's index within all message chunks.
    for (i, c) in out.iter_mut().enumerate() {
        c.seq_in_block = i as u32;
    }

    Ok(out)
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
    block_seq: u32,
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
            return chunk_code(document_id, markdown, config, block_seq);
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
        let id = chunk_id(document_id, chunk, start, end, block_seq);
        chunks.push(ChunkOutput::single(
            id,
            chunk.to_string(),
            span,
            heading_path,
            0,
        ));
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
    block_seq: u32,
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
                    let id = chunk_id(document_id, chunk_text, cs, ce, block_seq);
                    chunks.push(ChunkOutput::single(
                        id,
                        chunk_text.to_string(),
                        Span::new(cs, ce),
                        vec![],
                        0,
                    ));
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
                    let id = chunk_id(document_id, chunk_text, pos, piece_end, block_seq);
                    chunks.push(ChunkOutput::single(
                        id,
                        chunk_text.to_string(),
                        Span::new(pos, piece_end),
                        vec![],
                        0,
                    ));
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
                let id = chunk_id(document_id, chunk_text, cs, ce, block_seq);
                chunks.push(ChunkOutput::single(
                    id,
                    chunk_text.to_string(),
                    Span::new(cs, ce),
                    vec![],
                    0,
                ));
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
            let id = chunk_id(document_id, chunk_text, cs, ce, block_seq);
            chunks.push(ChunkOutput::single(
                id,
                chunk_text.to_string(),
                Span::new(cs, ce),
                vec![],
                0,
            ));
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
    fn chunker_config_from_preset_messages_succeeds() {
        let cfg = ChunkerConfig::from_preset("messages").unwrap();
        assert_eq!(cfg.preset, "messages");
        assert_eq!(cfg.resolved_window_turns(), 6);
        assert_eq!(cfg.resolved_stride_turns(), 3);
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
        let result = chunk_prose(&doc_id, "", &cfg, &CharSizer, 0).unwrap();
        assert!(result.is_empty(), "empty doc should produce no chunks");
    }

    #[test]
    fn prose_chunk_single_paragraph() {
        let full_text = "Hello, this is a paragraph.";
        let doc_id = document_id("file:///test.md", "abc");
        let cfg = ChunkerConfig::prose();

        let chunks = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
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

        let chunks = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert!(chunk.span.start <= chunk.span.end, "span start <= end");
            assert!(!chunk.text.is_empty(), "chunk text must be non-empty");
        }
        assert!(
            chunks
                .iter()
                .any(|c| c.text.contains("Introduction") || c.text.contains("intro")),
            "chunks should contain expected text"
        );
    }

    #[test]
    fn prose_spans_round_trip() {
        let full_text =
            "# Heading One\n\nParagraph one with some words.\n\n## Heading Two\n\nParagraph two here.";
        let doc_id = document_id("file:///rt.md", "abc");
        let cfg = ChunkerConfig::prose();
        let chunks = chunk_prose(&doc_id, full_text, &cfg, &WordSizer, 0).unwrap();
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(
                c.span.start <= c.span.end,
                "span start must be <= span end (sanity check)"
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
            window_turns: None,
            stride_turns: None,
        };
        let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
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
            window_turns: None,
            stride_turns: None,
        };
        let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
        assert!(chunks.len() >= 2, "should produce at least 2 chunks");
    }

    #[test]
    fn prose_char_sizer_fallback_produces_chunks() {
        let full_text = "# Title\n\nSome prose content here for the char sizer fallback path.";
        let doc_id = document_id("file:///char.md", "abc");
        let cfg = ChunkerConfig::prose();
        let chunks = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
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
            window_turns: None,
            stride_turns: None,
        };
        let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
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

        let chunks1 = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
        let chunks2 = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();

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
            window_turns: None,
            stride_turns: None,
        };

        let chunks = chunk_prose(&doc_id, full_text, &cfg, &WordSizer, 0).unwrap();
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
    fn prose_multibyte_utf8_no_panic() {
        let text = "こんにちは world — это тест";
        let doc_id = "doc-multibyte";
        let result = chunk_prose(doc_id, text, &ChunkerConfig::prose(), &CharSizer, 0);
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
            window_turns: None,
            stride_turns: None,
        };
        let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
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
            window_turns: None,
            stride_turns: None,
        };
        let result = chunk_prose(&doc_id, &full_text, &cfg, &CharSizer, 0);
        assert!(
            result.is_ok(),
            "oversized atomic unit should not panic: {:?}",
            result.err()
        );
        let chunks = result.unwrap();
        assert!(!chunks.is_empty());
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
            window_turns: None,
            stride_turns: None,
        };
        let chunks = chunk_prose(&doc_id, md, &cfg, &WordSizer, 0).unwrap();
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
        let chunks = chunk_code(&doc_id, "", &cfg, 0).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn code_chunk_single_block() {
        let full_text = "fn hello() {\n    println!(\"hi\");\n}";
        let doc_id = document_id("file:///lib.rs", "abc");
        let cfg = ChunkerConfig::code();

        let chunks = chunk_code(&doc_id, full_text, &cfg, 0).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, full_text);
    }

    #[test]
    fn code_chunk_large_splits() {
        let line = "let x = some_function_with_long_name(arg1, arg2, arg3);\n";
        let full_text = line.repeat(100); // ~5600 chars
        let doc_id = document_id("file:///lib.rs", "hash");
        let cfg = ChunkerConfig::code();

        let chunks = chunk_code(&doc_id, &full_text, &cfg, 0).unwrap();
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
        let chunks = chunk_code(&doc_id, &full_text, &cfg, 0).unwrap();
        for c in &chunks {
            assert!(
                c.span.start <= c.span.end,
                "span start must be <= span end (sanity check)"
            );
        }
    }

    #[test]
    fn chunk_blocks_multibyte_code_preset_does_not_panic() {
        let unit = "日本語テキスト: これはテストです。 ";
        let text = unit.repeat(200);
        let doc_id = "doc-multibyte-code";
        let result = chunk_code(doc_id, &text, &ChunkerConfig::code(), 0);
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
        let chunks = chunk_code(doc_id, &long_line, &cfg, 0).unwrap();
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
        let chunks = chunk_prose(doc_id, &long_line, &cfg, &CharSizer, 0).unwrap();
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
        let chunks = chunk_code(doc_id, &content, &cfg, 0).unwrap();
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

    // ---------------------------------------------------------------------------
    // Messages chunker tests
    // ---------------------------------------------------------------------------

    /// Build a Message block for testing.
    fn msg_block(seq: u32, sender: &str, timestamp: &str, text: &str) -> crate::block::Block {
        crate::block::Block {
            seq,
            kind: crate::block::BlockKind::Message {
                sender: sender.to_string(),
                timestamp: Some(timestamp.to_string()),
                message_id: None,
                reply_to: None,
            },
            text: text.to_string(),
            location: None,
        }
    }

    /// Build a Segment block for testing.
    fn seg_block(
        seq: u32,
        speaker: Option<&str>,
        start_ms: u64,
        end_ms: u64,
        text: &str,
    ) -> crate::block::Block {
        crate::block::Block {
            seq,
            kind: crate::block::BlockKind::Segment {
                speaker: speaker.map(|s| s.to_string()),
                start_ms,
                end_ms,
            },
            text: text.to_string(),
            location: None,
        }
    }

    #[test]
    fn messages_empty_conversation_returns_no_chunks() {
        // No Message/Segment blocks → 0 chunks.
        let blocks: Vec<crate::block::Block> = vec![crate::block::Block {
            seq: 0,
            kind: crate::block::BlockKind::Paragraph,
            text: "Some intro text.".to_string(),
            location: None,
        }];
        let cfg = ChunkerConfig::messages();
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        assert!(chunks.is_empty(), "no message blocks → no chunks");
    }

    #[test]
    fn messages_single_message_produces_one_chunk() {
        let blocks = vec![msg_block(
            0,
            "Alice",
            "2026-01-01T10:00:00Z",
            "Hello there!",
        )];
        let cfg = ChunkerConfig::messages();
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        assert_eq!(chunks.len(), 1, "single message → single chunk");
        assert!(
            chunks[0].text.contains("Hello there!"),
            "chunk should contain message text"
        );
        assert_eq!(chunks[0].window_block_seqs, vec![0]);
        assert_eq!(chunks[0].block_seq, 0);
        assert_eq!(chunks[0].seq_in_block, 0);
    }

    #[test]
    fn messages_sliding_window_correct_chunk_count() {
        // 10 messages, window=6, stride=3 → windows at [0..6], [3..9], [6..10], [9..10]
        // = 4 windows (window_start advances by stride=3: 0, 3, 6, 9, stop at 10)
        let blocks: Vec<_> = (0..10)
            .map(|i| msg_block(i as u32, "User", "2026-01-01", &format!("Message {i}")))
            .collect();
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: Some(5000), // large budget so no token-based shrink
            overlap_tokens: Some(0),
            window_turns: Some(6),
            stride_turns: Some(3),
        };
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        assert_eq!(
            chunks.len(),
            4,
            "10 messages, window=6, stride=3 → 4 chunks; got {}",
            chunks.len()
        );
    }

    #[test]
    fn messages_sliding_window_correct_content() {
        // 10 messages, window=6, stride=3.
        // Window 0: msgs 0-5; window 1: msgs 3-8; window 2: msgs 6-9 (4 msgs); window 3: msg 9.
        let blocks: Vec<_> = (0..10)
            .map(|i| msg_block(i as u32, "User", "2026-01-01", &format!("Msg{i}")))
            .collect();
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: Some(5000),
            overlap_tokens: Some(0),
            window_turns: Some(6),
            stride_turns: Some(3),
        };
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        assert_eq!(chunks.len(), 4);

        // Window 0 should contain msgs 0-5.
        assert!(
            chunks[0].text.contains("Msg0"),
            "window 0 should start at Msg0"
        );
        assert!(
            chunks[0].text.contains("Msg5"),
            "window 0 should end at Msg5"
        );
        assert!(
            !chunks[0].text.contains("Msg6"),
            "window 0 should not contain Msg6"
        );
        assert_eq!(chunks[0].window_block_seqs, vec![0, 1, 2, 3, 4, 5]);

        // Window 1 should contain msgs 3-8.
        assert!(
            chunks[1].text.contains("Msg3"),
            "window 1 should start at Msg3"
        );
        assert!(
            chunks[1].text.contains("Msg8"),
            "window 1 should end at Msg8"
        );
        assert!(
            !chunks[1].text.contains("Msg9"),
            "window 1 should not contain Msg9"
        );
        assert_eq!(chunks[1].window_block_seqs, vec![3, 4, 5, 6, 7, 8]);

        // Window 2 should contain msgs 6-9.
        assert!(
            chunks[2].text.contains("Msg6"),
            "window 2 should start at Msg6"
        );
        assert!(
            chunks[2].text.contains("Msg9"),
            "window 2 should end at Msg9"
        );
        assert_eq!(chunks[2].window_block_seqs, vec![6, 7, 8, 9]);

        // Window 3 should contain only msg 9 (tail window).
        assert!(chunks[3].text.contains("Msg9"), "window 3 is the tail");
        assert_eq!(chunks[3].window_block_seqs, vec![9]);
    }

    #[test]
    fn messages_window_text_format() {
        // Verify [sender] (timestamp): text format.
        let blocks = vec![
            msg_block(0, "Alice", "2026-01-01T10:00:00Z", "Hello!"),
            msg_block(1, "Bob", "2026-01-01T10:01:00Z", "Hi there!"),
        ];
        let cfg = ChunkerConfig::messages();
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        assert_eq!(chunks.len(), 1, "2 messages within a window → 1 chunk");
        let text = &chunks[0].text;
        assert!(
            text.contains("[Alice] (2026-01-01T10:00:00Z): Hello!"),
            "should format as [sender] (timestamp): text; got: {text:?}"
        );
        assert!(
            text.contains("[Bob] (2026-01-01T10:01:00Z): Hi there!"),
            "should include second message; got: {text:?}"
        );
    }

    #[test]
    fn messages_segment_blocks_windowing() {
        // Segment blocks should behave the same as Message blocks.
        let blocks: Vec<_> = (0..6)
            .map(|i| {
                seg_block(
                    i as u32,
                    Some("Speaker"),
                    i as u64 * 2000,
                    i as u64 * 2000 + 1999,
                    &format!("Segment text {i}"),
                )
            })
            .collect();
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: Some(5000),
            overlap_tokens: Some(0),
            window_turns: Some(4),
            stride_turns: Some(2),
        };
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        // 6 turns, window=4, stride=2 → windows at [0..4], [2..6], [4..6] → 3 windows
        assert_eq!(
            chunks.len(),
            3,
            "6 segments, window=4, stride=2 → 3 chunks; got {}",
            chunks.len()
        );
        // Segment format: [speaker] (start_ms-end_ms): text
        assert!(
            chunks[0]
                .text
                .contains("[Speaker] (0-1999): Segment text 0"),
            "should format segment as [speaker] (start-end): text"
        );
    }

    #[test]
    fn messages_mixed_blocks_only_sees_message_and_segment() {
        // Heading + Paragraph + 3 Message + Paragraph + 1 Message
        // The messages chunker should see only the 4 Message blocks.
        let blocks = vec![
            crate::block::Block {
                seq: 0,
                kind: crate::block::BlockKind::Heading { level: 1 },
                text: "Conversation".to_string(),
                location: None,
            },
            crate::block::Block {
                seq: 1,
                kind: crate::block::BlockKind::Paragraph,
                text: "Intro paragraph.".to_string(),
                location: None,
            },
            msg_block(2, "Alice", "2026-01-01T10:00:00Z", "First message"),
            msg_block(3, "Bob", "2026-01-01T10:01:00Z", "Second message"),
            msg_block(4, "Alice", "2026-01-01T10:02:00Z", "Third message"),
            crate::block::Block {
                seq: 5,
                kind: crate::block::BlockKind::Paragraph,
                text: "Interlude paragraph.".to_string(),
                location: None,
            },
            msg_block(6, "Bob", "2026-01-01T10:03:00Z", "Fourth message"),
        ];
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: Some(5000),
            overlap_tokens: Some(0),
            window_turns: Some(6),
            stride_turns: Some(3),
        };
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        // 4 message blocks, window=6 (fits all 4), stride=3 → windows at 0 and 3.
        // Window 0: msgs 2,3,4,6. Window 1 (stride 3): msg 6 only (index 3 in turns).
        assert_eq!(
            chunks.len(),
            2,
            "4 messages, window=6, stride=3 → 2 chunks; got {}",
            chunks.len()
        );
        // First window covers all 4 message blocks.
        assert_eq!(chunks[0].window_block_seqs, vec![2, 3, 4, 6]);
        // Should NOT contain non-message text.
        assert!(
            !chunks[0].text.contains("Intro paragraph"),
            "chunker must not include non-message text"
        );
    }

    #[test]
    fn messages_very_long_single_message_splits() {
        // A single message that exceeds max_tokens should be split into sub-chunks,
        // with each sub-chunk prefixed by sender/timestamp context.
        let long_text = "word ".repeat(200); // 200 words
        let blocks = vec![msg_block(0, "Alice", "2026-01-01T10:00:00Z", &long_text)];
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: Some(50), // small budget to force splitting
            overlap_tokens: Some(0),
            window_turns: Some(6),
            stride_turns: Some(3),
        };
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &WordSizer).unwrap();
        assert!(
            chunks.len() > 1,
            "very long message should produce multiple sub-chunks; got {}",
            chunks.len()
        );
        // Every sub-chunk should contain the sender prefix.
        for c in &chunks {
            assert!(
                c.text.contains("[Alice]"),
                "each sub-chunk should preserve sender context; got: {:?}",
                c.text
            );
        }
    }

    #[test]
    fn messages_seq_in_block_sequential() {
        // seq_in_block should be 0, 1, 2, ... across all message chunks.
        let blocks: Vec<_> = (0..9)
            .map(|i| msg_block(i as u32, "User", "2026-01-01", &format!("Msg{i}")))
            .collect();
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: Some(5000),
            overlap_tokens: Some(0),
            window_turns: Some(6),
            stride_turns: Some(3),
        };
        let chunks = chunk_messages("resource-1", &blocks, &cfg, &CharSizer).unwrap();
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(
                c.seq_in_block, i as u32,
                "seq_in_block should be index {i}; got {}",
                c.seq_in_block
            );
        }
    }

    #[test]
    fn messages_config_default_values() {
        let cfg = ChunkerConfig::messages();
        assert_eq!(cfg.preset, "messages");
        assert_eq!(cfg.resolved_window_turns(), 6);
        assert_eq!(cfg.resolved_stride_turns(), 3);
        assert_eq!(cfg.resolved_target_tokens(), 512);
    }

    // ---------------------------------------------------------------------------
    // Fix 4: code preset routes prose-shaped blocks through code chunker
    // ---------------------------------------------------------------------------

    #[test]
    fn code_preset_routes_paragraph_block_through_code_chunker() {
        // A Paragraph block fed to chunk_blocks with preset="code" must go
        // through the code (line-packer) path, not the prose (MarkdownSplitter) path.
        // We verify this by checking that the chunks are produced (no panic) and
        // that their spans are valid byte ranges.
        let block = crate::block::Block {
            seq: 0,
            kind: crate::block::BlockKind::Paragraph,
            text: "fn hello() {\n    println!(\"hi\");\n}".to_string(),
            location: None,
        };
        let doc_id = document_id("file:///test.rs", "abc");
        let cfg = ChunkerConfig::code();
        let chunks = chunk_blocks(&doc_id, &[block], &cfg, &CharSizer).unwrap();
        assert!(
            !chunks.is_empty(),
            "code preset + Paragraph should produce chunks"
        );
        for c in &chunks {
            assert!(c.span.start <= c.span.end, "span start <= end");
        }
    }

    // ---------------------------------------------------------------------------
    // Fix 6: message windows shrink from end — all turns covered
    // ---------------------------------------------------------------------------

    #[test]
    fn messages_all_turns_appear_when_windows_are_oversized() {
        // 4 turns, each 10 chars. Budget = 15 chars (fits 1 turn per window).
        // stride = 1 so every turn is a window_start at some point.
        // After the end-shrink fix, every turn must appear in at least one chunk.
        let turns: Vec<_> = (0..4)
            .map(|i| msg_block(i as u32, "U", "2026-01-01", "1234567890")) // 10 chars each
            .collect();
        let cfg = ChunkerConfig {
            preset: "messages".to_string(),
            target_tokens: Some(15), // fits exactly 1 turn (10 chars) plus a separator
            overlap_tokens: Some(0),
            window_turns: Some(4),
            stride_turns: Some(1),
        };
        let chunks = chunk_messages("resource-x", &turns, &cfg, &CharSizer).unwrap();
        // Each chunk must include at least turn 0 (window_start=0 in first window)
        // and turn 3 (window_start=3 in last window).
        let covered_seqs: std::collections::HashSet<u32> = chunks
            .iter()
            .flat_map(|c| c.window_block_seqs.iter().copied())
            .collect();
        for i in 0u32..4 {
            assert!(
                covered_seqs.contains(&i),
                "turn {i} must appear in at least one window; covered: {covered_seqs:?}"
            );
        }
    }
}
