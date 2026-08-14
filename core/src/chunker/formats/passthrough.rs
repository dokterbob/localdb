//! Pass-through chunker: single-chunk placeholder for blocks with no internal structure to
//! split further (`Reference`, `Attachment`, `Frontmatter`, `Image`).

use crate::block::{Block, BlockKind};
use crate::chunker::output::ChunkOutput;
use crate::chunker::ChunkerConfig;
use crate::types::Span;
use crate::Error;

use super::{chunk_each, ChunkContext, FormatChunker, GroupScope};

// ---------------------------------------------------------------------------
// Pass-through chunker
// ---------------------------------------------------------------------------

/// `FormatChunker` for blocks with no internal structure: the whole block's text becomes a
/// single placeholder chunk. NOT a catch-all — a new `BlockKind` not listed in `claims` is
/// claimed by no format, and `chunk_blocks` fails loudly rather than silently dropping it.
pub(in crate::chunker) struct Passthrough;

impl FormatChunker for Passthrough {
    fn name(&self) -> &'static str {
        "passthrough"
    }

    fn scope(&self) -> GroupScope {
        GroupScope::Run
    }

    fn claims(&self, block: &Block, _config: &ChunkerConfig) -> bool {
        matches!(
            block.kind,
            BlockKind::Reference { .. }
                | BlockKind::Attachment { .. }
                | BlockKind::Frontmatter { .. }
                | BlockKind::Image { .. }
        )
    }

    fn chunk(&self, ctx: &ChunkContext<'_>, blocks: &[&Block]) -> Result<Vec<ChunkOutput>, Error> {
        chunk_each(ctx, blocks, |_ctx, block| {
            let text = &block.text;
            Ok(vec![ChunkOutput::placeholder(
                text.clone(),
                Span::new(0, text.len()),
                vec![],
                block.seq,
                0,
            )])
        })
    }
}
