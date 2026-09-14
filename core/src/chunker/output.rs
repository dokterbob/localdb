//! `ChunkOutput` — one chunk produced by a chunker, plus id finalization.

use crate::ids::{chunk_id, ContentId};
use crate::types::Span;

/// A single chunk produced by the chunker.
///
/// The `id` is content-addressed; the `text` and `span` refer to the normalized
/// Markdown string. `heading_path` is derived from the Markdown heading structure.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkOutput {
    /// Content-addressed chunk ID: `blake3(resource_id || text || span)`.
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
    /// Block kind string (e.g. "text", "heading"). `None` for flat-document chunks.
    pub block_kind: Option<String>,
}

impl ChunkOutput {
    /// Construct a single-block, non-windowed chunk with a placeholder id.
    ///
    /// Convenience constructor that reduces boilerplate in block-dispatch paths. The `id`
    /// field is left as an empty placeholder — callers MUST run [`finalize_ids`] over the
    /// batch once `block_seq`/`seq_in_block` are final (see the module-level note on
    /// [`finalize_ids`]).
    pub(in crate::chunker) fn placeholder(
        text: String,
        span: Span,
        heading_path: Vec<String>,
        block_seq: u32,
        seq_in_block: u32,
    ) -> Self {
        Self {
            id: ContentId::new(),
            text,
            span,
            heading_path,
            block_seq,
            seq_in_block,
            window_block_seqs: vec![],
            block_kind: None,
        }
    }
}

/// Compute and assign the final content-addressed `id` for every chunk in `chunks`.
///
/// Chunk ids are `blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)`
/// ([`crate::ids::chunk_id`]) — deliberately NOT based on span. `block_seq` and
/// `seq_in_block` must already hold their FINAL values when this runs: for ordinary
/// block-dispatched chunks that means after `chunk_blocks`'s per-block loop has assigned
/// them; for message-window chunks that means after `chunk_messages`'s end-of-sequence
/// fix-up pass. Calling this more than once on the same (unchanged) chunks is safe —
/// finalization is idempotent, since the id is a pure function of fields that don't change
/// afterward.
pub(in crate::chunker) fn finalize_ids(resource_id: &str, chunks: &mut [ChunkOutput]) {
    for c in chunks.iter_mut() {
        c.id = chunk_id(resource_id, c.block_seq, &c.text, c.seq_in_block);
    }
}
