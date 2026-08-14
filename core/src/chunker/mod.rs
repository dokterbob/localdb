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

use formats::code::chunk_code;
use formats::prose::chunk_prose;
use formats::table::chunk_table;
use output::finalize_ids;

use crate::Error;

// ---------------------------------------------------------------------------
// Block-aware chunk function
// ---------------------------------------------------------------------------

/// Chunk a sequence of typed [`Block`]s into `ChunkOutput` records.
///
/// Dispatches by block kind:
/// - `Message`, `Segment` → messages chunker (sliding window over all such blocks).
/// - `Heading`, `Text` → prose chunker (per block).
/// - `Code` → code chunker (per block).
/// - `Table` → table chunker (row-based packer; falls back to the code chunker for
///   malformed tables — see [`chunk_table`]).
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
            BlockKind::Heading { .. } | BlockKind::Text => {
                if config.preset == "code" {
                    chunk_code(resource_id, &block.text, config, block.seq)?
                } else {
                    chunk_prose(resource_id, &block.text, config, sizer, block.seq)?
                }
            }
            // Code blocks
            BlockKind::Code { .. } => chunk_code(resource_id, &block.text, config, block.seq)?,
            // Table blocks: dedicated row-based packer (specs/04-search-pipeline.md §3).
            BlockKind::Table { .. } => {
                chunk_table(resource_id, &block.text, config, sizer, block.seq)?
            }
            // Single-block pass-through
            BlockKind::Reference { .. }
            | BlockKind::Attachment { .. }
            | BlockKind::Frontmatter { .. }
            | BlockKind::Image { .. } => {
                let text = &block.text;
                vec![ChunkOutput::placeholder(
                    text.clone(),
                    crate::types::Span::new(0, text.len()),
                    heading_path.clone(),
                    block.seq,
                    0,
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

    // Final pass: every chunk's block_seq/seq_in_block is now settled (block-dispatched
    // chunks were just finalized above; message-window chunks were finalized inside
    // `chunk_messages` after its own end-of-sequence fix-up). Compute ids here too —
    // idempotent for chunks that are already finalized, and the only place ids are
    // assigned for the single-block pass-through kinds.
    finalize_ids(resource_id, &mut out);

    Ok(out)
}
