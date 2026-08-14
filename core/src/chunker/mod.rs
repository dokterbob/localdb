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
//! - `table` (dispatched by `BlockKind::Table`, not a source-level preset): row-based packer
//!   that re-emits the header/separator row per chunk; see [`chunk_table`].
//!
//! Heading-path attribution uses `heading_index::build_heading_index` internally
//! within `chunk_prose` over the real Markdown string.
//!
//! **Chunk id finalization:** every chunk's content-addressed `id` (`crate::ids::chunk_id`)
//! is a function of its FINAL `block_seq`/`seq_in_block`, not of span. Chunks are built with
//! a placeholder id and only assigned a real one by [`finalize_ids`], once those two fields
//! can no longer change — see the doc comment on `finalize_ids` for the exact points where
//! that happens for each chunker.

mod config;
mod formats;
mod output;
mod preset;
mod sizers;

#[cfg(test)]
mod tests;

pub use config::ChunkerConfig;
pub use formats::messages::chunk_messages;
pub use output::ChunkOutput;
pub use preset::preset_for;
pub use sizers::{CharSizer, ChunkSizer, TokenSizer};

use formats::{ChunkContext, GroupScope, FORMATS};
use output::finalize_ids;

use crate::Error;

// ---------------------------------------------------------------------------
// Block-aware chunk function
// ---------------------------------------------------------------------------

/// Chunk a sequence of typed [`Block`]s into `ChunkOutput` records.
///
/// Dispatches each non-empty block to the first [`formats::FormatChunker`] in
/// [`formats::FORMATS`] whose `claims` returns true for it (registry order encodes claim
/// precedence — see that constant's doc comment):
/// - `Message`, `Segment` → messages chunker (sliding window over all such blocks,
///   document-scoped: runs once over the whole doc, ahead of everything else).
/// - `Heading`, `Text` → code chunker when `config.preset == "code"`, else prose chunker.
/// - `Code` → code chunker.
/// - `Table` → table chunker (row-based packer; falls back to the code chunker for
///   malformed tables).
/// - `Reference`, `Attachment`, `Frontmatter`, `Image` → single chunk per block.
///
/// For each sub-chunk within a block:
/// - `block_seq` is set to `block.seq`.
/// - `seq_in_block` is set to the chunk's index within that block.
/// - `heading_path` is derived from `heading_path_from_blocks`.
///
/// Blocks with empty text are skipped. A block claimed by no format is a bug (a new
/// `BlockKind` added without a `FormatChunker` to handle it) — this fails loudly rather
/// than silently dropping content.
pub fn chunk_blocks(
    resource_id: &str,
    blocks: &[crate::block::Block],
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    let ctx = ChunkContext {
        resource_id,
        config,
        sizer,
        blocks,
    };

    // Pair every non-empty block with the index of the FIRST format in `FORMATS` that
    // claims it.
    let claimed: Vec<(&crate::block::Block, usize)> = blocks
        .iter()
        .filter(|b| !b.text.is_empty())
        .map(|b| {
            let idx = FORMATS
                .iter()
                .position(|f| f.claims(b, config))
                .unwrap_or_else(|| {
                    unreachable!("block kind {:?} claimed by no FormatChunker", b.kind)
                });
            (b, idx)
        })
        .collect();

    let mut out: Vec<ChunkOutput> = Vec::new();

    // Document-scoped formats (today: only `Messages`) run once over the whole doc, ahead
    // of everything else — same as the old dispatch's first pass. Iterating `FORMATS` in
    // registry order keeps this deterministic if more Document-scoped formats appear later.
    for (fmt_idx, fmt) in FORMATS.iter().enumerate() {
        if matches!(fmt.scope(), GroupScope::Document)
            && claimed.iter().any(|(_, idx)| *idx == fmt_idx)
        {
            tracing::trace!(
                format = fmt.name(),
                "chunk_blocks: document-scoped dispatch"
            );
            out.extend(fmt.chunk(&ctx, &[])?);
        }
    }

    // Remaining (Run-scoped) blocks: walk in doc order, partitioning into maximal runs of
    // consecutive same-format blocks so each format's `chunk` sees a contiguous group.
    // Per-block formats chunk each block independently inside `chunk_each`, so the
    // concatenation across runs is identical to dispatching block-by-block.
    let mut i = 0;
    while i < claimed.len() {
        let (_, idx) = claimed[i];
        if matches!(FORMATS[idx].scope(), GroupScope::Document) {
            i += 1;
            continue;
        }
        let start = i;
        while i < claimed.len() && claimed[i].1 == idx {
            i += 1;
        }
        let run: Vec<&crate::block::Block> = claimed[start..i].iter().map(|(b, _)| *b).collect();
        tracing::trace!(
            format = FORMATS[idx].name(),
            block_count = run.len(),
            "chunk_blocks: run-scoped dispatch"
        );
        out.extend(FORMATS[idx].chunk(&ctx, &run)?);
    }

    // Final pass: every chunk's block_seq/seq_in_block is now settled (block-dispatched
    // chunks were stamped by `chunk_each` above; message-window chunks were finalized
    // inside `chunk_messages` after its own end-of-sequence fix-up). Compute ids here too —
    // idempotent for chunks that are already finalized, and the only place ids are
    // assigned for the single-block pass-through kinds.
    finalize_ids(resource_id, &mut out);

    Ok(out)
}
