//! Per-source-format chunking implementations, dispatched from `chunk_blocks` via the
//! [`FormatChunker`] trait: each block goes to the first format in the [`FORMATS`] registry
//! whose `claims` returns true (registry order = claim precedence), grouped per format's
//! [`GroupScope`] before `chunk` runs.

pub(in crate::chunker) mod code;
pub(in crate::chunker) mod messages;
pub(in crate::chunker) mod passthrough;
pub(in crate::chunker) mod prose;
pub(in crate::chunker) mod table;

#[cfg(test)]
mod tests;

use crate::block::Block;
use crate::chunker::output::ChunkOutput;
use crate::chunker::sizers::ChunkSizer;
use crate::chunker::ChunkerConfig;
use crate::markdown_blocks::heading_path_from_blocks;
use crate::Error;

/// Everything a [`FormatChunker`] needs to chunk its claimed blocks.
pub(in crate::chunker) struct ChunkContext<'a> {
    pub resource_id: &'a str,
    pub config: &'a ChunkerConfig,
    pub sizer: &'a dyn ChunkSizer,
    /// The full document's blocks — needed by formats that derive heading paths or
    /// (for `Messages`) window across the whole document rather than a claimed subset.
    pub blocks: &'a [Block],
}

/// How a format's claimed blocks are grouped before [`FormatChunker::chunk`] is invoked.
pub(in crate::chunker) enum GroupScope {
    /// All claimed blocks document-wide as one group (messages: windows span the doc's
    /// turns — the explicit "chunk ⊂ block" exception, specs/04-search-pipeline.md §3).
    Document,
    /// Maximal runs of consecutive claimed blocks (everything else). Deliberate seam for
    /// #158 section-aware prose packing.
    Run,
}

/// A pluggable per-source-format chunker, dispatched from `chunk_blocks` via [`FORMATS`].
pub(in crate::chunker) trait FormatChunker {
    /// Short identifier for the format, e.g. `"prose"`.
    fn name(&self) -> &'static str;

    /// How this format's claimed blocks are grouped before `chunk` runs.
    fn scope(&self) -> GroupScope;

    /// Does this format handle `block` under `config`? Encodes today's routing, including
    /// "Heading/Text route to Code when `config.preset == \"code\"`". Blocks are dispatched
    /// to the FIRST format in [`FORMATS`] whose `claims` returns true.
    fn claims(&self, block: &Block, config: &ChunkerConfig) -> bool;

    /// Chunk one claimed group (blocks in doc order).
    fn chunk(&self, ctx: &ChunkContext<'_>, blocks: &[&Block]) -> Result<Vec<ChunkOutput>, Error>;
}

/// Format registry, in claim-precedence order: a block is dispatched to the FIRST format
/// here whose `claims` returns true. `Code` MUST precede `Prose` so that Heading/Text blocks
/// under `config.preset == "code"` are claimed by `Code` rather than falling through to
/// `Prose`'s unconditional Heading/Text claim.
pub(in crate::chunker) const FORMATS: [&dyn FormatChunker; 5] = [
    &messages::Messages,
    &code::Code,
    &prose::Prose,
    &table::Table,
    &passthrough::Passthrough,
];

/// Chunk each block in `blocks` independently via `f`, then stamp every returned chunk with
/// `block_seq`, `seq_in_block`, `block_kind`, and (when the chunk didn't already set one) a
/// `heading_path` derived from `heading_path_from_blocks`. Shared by every `Run`-scoped
/// format's `chunk` impl — this is the per-chunk stamping that used to live inline in
/// `chunk_blocks`'s per-block loop.
pub(in crate::chunker) fn chunk_each<F>(
    ctx: &ChunkContext<'_>,
    blocks: &[&Block],
    f: F,
) -> Result<Vec<ChunkOutput>, Error>
where
    F: Fn(&Block) -> Result<Vec<ChunkOutput>, Error>,
{
    let mut out = Vec::new();
    for block in blocks {
        let heading_path = heading_path_from_blocks(ctx.blocks, block.seq);
        let sub_chunks = f(block)?;
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
