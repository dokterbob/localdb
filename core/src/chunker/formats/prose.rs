//! Prose chunker: Markdown-structure-aware split via `text-splitter`.

use crate::block::{Block, BlockKind};
use crate::chunker::output::{finalize_ids, ChunkOutput};
use crate::chunker::sizers::{ChunkSizer, TsSizer};
use crate::chunker::ChunkerConfig;
use crate::types::Span;
use crate::Error;

use super::code::chunk_code;
use super::{chunk_each, ChunkContext, FormatChunker, GroupScope};

/// `chunk_prose`'s Layer D backstop threshold multiplier: a block is delegated to
/// `chunk_code` when its longest whitespace-free run exceeds this many multiples of the
/// char target. See the doc comment at the backstop's call site in `chunk_prose`.
pub(in crate::chunker) const STRUCTURELESS_RUN_MULTIPLIER: usize = 8;

/// `chunk_prose`'s Layer D performance guard multiplier: a block is also delegated to
/// `chunk_code` when its longest *line* exceeds this many multiples of the target,
/// regardless of internal whitespace. `MarkdownSplitter`'s split-point search is
/// super-linear on a single flat line (the multi-minute-hang class the backstop was
/// introduced for in #61); real prose paragraphs — even the single-line ones EPUB/HTML
/// extraction emits — stay far below this cap, so they keep the semantic prose path.
pub(in crate::chunker) const OVERLONG_LINE_MULTIPLIER: usize = 64;

// ---------------------------------------------------------------------------
// Prose chunker
// ---------------------------------------------------------------------------

/// Prose chunker: Markdown-structure-aware split via `text-splitter`.
///
/// Feeds REAL Markdown (with `#`, fences, bullets) to `MarkdownSplitter`,
/// fixing the latent smell where stripped text was passed before.
/// Heading-path attribution uses `heading_index::build_heading_index` over the
/// same Markdown string — no Block sidecar needed.
pub(in crate::chunker) fn chunk_prose(
    resource_id: &str,
    markdown: &str,
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
    block_seq: u32,
) -> Result<Vec<ChunkOutput>, Error> {
    if markdown.is_empty() {
        return Ok(vec![]);
    }

    let target = config.resolved_target_tokens();

    // Layer D: backstop for structureless files misclassified as prose. Two independent
    // probes, each a single O(n) pass; tripping either delegates the block to `chunk_code`:
    //
    // 1. Quality probe — longest whitespace-free run > STRUCTURELESS_RUN_MULTIPLIER ×
    //    target. An ordinary long paragraph (e.g. from EPUB/HTML extraction, which emits
    //    paragraphs as single long lines) has plenty of internal whitespace and should
    //    NOT be diverted to the char-level `chunk_code` splitter — only genuinely
    //    structureless content (minified JSON, lockfiles) has no whitespace to break on.
    //
    // 2. Performance guard — longest line > OVERLONG_LINE_MULTIPLIER × target, whitespace
    //    or not. `MarkdownSplitter` is super-linear on one flat line (the #61 hang class),
    //    so a pathologically long single line (hundreds of KB of space-separated tokens)
    //    must not reach it. Routing it to `chunk_code` is acceptable since the hard-split
    //    there backs off to whitespace: even the degraded path cuts between words.
    //
    // Accepted limitations, both deliberate (no per-token special-casing):
    // - A paragraph containing one giant space-free token (a URL, a base64 blob) trips
    //   probe 1 and sends the WHOLE block to `chunk_code`, even though the rest is
    //   ordinary prose — that token is unsplittable-without-mid-token-cuts anyway.
    // - Scripts without inter-word whitespace (CJK, Thai, …) make the whole paragraph one
    //   "run", so long CJK prose trips probe 1 and gets char-aligned cuts in `chunk_code`
    //   (same as before this probe existed); proper word segmentation is out of scope.
    {
        let max_run_len = markdown
            .split_whitespace()
            .map(|w| w.chars().count())
            .max()
            .unwrap_or(0);
        let max_line_len = markdown
            .lines()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        let run_threshold = STRUCTURELESS_RUN_MULTIPLIER * target;
        let line_threshold = OVERLONG_LINE_MULTIPLIER * target;
        tracing::debug!(
            max_run_len,
            run_threshold,
            max_line_len,
            line_threshold,
            "chunk_prose backstop probe"
        );
        if max_run_len > run_threshold || max_line_len > line_threshold {
            tracing::debug!(
                max_run_len,
                max_line_len,
                "chunk_prose backstop: delegating to chunk_code"
            );
            return chunk_code(resource_id, markdown, config, block_seq);
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
    for (seq_in_block, (byte_off, chunk)) in splitter.chunk_indices(markdown).enumerate() {
        let start = byte_off;
        let end = byte_off + chunk.len();
        let span = Span::new(start, end);
        let heading_path = crate::heading_index::heading_path_at(&heading_idx, start);
        chunks.push(ChunkOutput::placeholder(
            chunk.to_string(),
            span,
            heading_path,
            block_seq,
            seq_in_block as u32,
        ));
    }

    // Ids depend on `block_seq`/`seq_in_block`, both final at this point (this function
    // owns the full ordering of its own output) — see `finalize_ids`.
    finalize_ids(resource_id, &mut chunks);

    Ok(chunks)
}

// ---------------------------------------------------------------------------
// FormatChunker impl
// ---------------------------------------------------------------------------

/// `FormatChunker` for prose-shaped blocks (`Heading`, `Text`) under non-code presets.
/// `Code` claims Heading/Text first when `config.preset == "code"` (registry precedence in
/// [`super::FORMATS`]), so this impl's `claims` doesn't need to check the preset itself.
pub(in crate::chunker) struct Prose;

impl FormatChunker for Prose {
    fn name(&self) -> &'static str {
        "prose"
    }

    fn scope(&self) -> GroupScope {
        GroupScope::Run
    }

    fn claims(&self, block: &Block, _config: &ChunkerConfig) -> bool {
        matches!(block.kind, BlockKind::Heading { .. } | BlockKind::Text)
    }

    fn chunk(&self, ctx: &ChunkContext<'_>, blocks: &[&Block]) -> Result<Vec<ChunkOutput>, Error> {
        chunk_each(ctx, blocks, |ctx, block| {
            chunk_prose(
                ctx.resource_id,
                &block.text,
                ctx.config,
                ctx.sizer,
                block.seq,
            )
        })
    }
}
