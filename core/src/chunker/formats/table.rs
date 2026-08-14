//! Table chunker: row-based packer for `Table` blocks.

use crate::chunker::output::{finalize_ids, ChunkOutput};
use crate::chunker::sizers::ChunkSizer;
use crate::chunker::ChunkerConfig;
use crate::types::Span;
use crate::Error;

use super::code::chunk_code;

// ---------------------------------------------------------------------------
// Table chunker
// ---------------------------------------------------------------------------

/// Table chunker: row-based packer for `Table` blocks (specs/04-search-pipeline.md §3).
///
/// Expects `markdown` to be a standalone Markdown pipe-table: a header row, a `|---|`-style
/// separator row, and zero or more data rows. Data rows are packed greedily into successive
/// chunks under the (prose) token target; EVERY chunk re-emits the header and separator rows,
/// so each chunk is an independently valid, renderable Markdown table — no chunk depends on a
/// sibling to parse correctly.
///
/// Falls back to [`chunk_code`] (the pre-table-chunker behavior) in two cases:
/// - The block's text has no recognizable header/separator row (malformed table) — the whole
///   block is routed through the line-based packer rather than guessing at structure.
/// - A single data row, combined with the re-emitted header+separator, still exceeds the
///   token target — it cannot be packed into a standalone valid chunk at the target size, so
///   that one row alone is split via `chunk_code`'s long-line logic (preserving the
///   "no unbounded chunk" invariant, at the cost of that fallback chunk not being a
///   well-formed table row).
pub(in crate::chunker) fn chunk_table(
    resource_id: &str,
    markdown: &str,
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
    block_seq: u32,
) -> Result<Vec<ChunkOutput>, Error> {
    if markdown.is_empty() {
        return Ok(vec![]);
    }

    let lines: Vec<&str> = markdown.lines().collect();

    // Header row: first non-blank line. Separator row: the line right after it.
    let header_idx = lines.iter().position(|l| !l.trim().is_empty());
    let parsed = header_idx.and_then(|hi| {
        let si = hi + 1;
        let sep_line = lines.get(si)?;
        if lines[hi].contains('|') && is_table_separator_row(sep_line) {
            Some((hi, si))
        } else {
            None
        }
    });

    let Some((header_idx, sep_idx)) = parsed else {
        // Malformed: no recognizable header/separator row. Fall back to the previous,
        // code-chunker-based behavior rather than guessing at table structure.
        return chunk_code(resource_id, markdown, config, block_seq);
    };

    let header_block = format!("{}\n{}", lines[header_idx], lines[sep_idx]);

    let data_rows: Vec<&str> = lines[(sep_idx + 1)..]
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();

    if data_rows.is_empty() {
        // Header-only table block: no data rows to pack, but the header itself is
        // real content and must not silently vanish from the index. Emit it as a
        // single chunk rather than falling through to `flush_table_batch` (which
        // treats an empty `pending` as "nothing to flush").
        let mut chunks = vec![ChunkOutput::placeholder(
            header_block,
            Span::new(0, 0),
            vec![],
            block_seq,
            0,
        )];
        finalize_ids(resource_id, &mut chunks);
        return Ok(chunks);
    }

    let target = config.resolved_target_tokens();
    let mut chunks: Vec<ChunkOutput> = Vec::new();
    let mut seq_in_block = 0u32;
    let mut pending: Vec<&str> = Vec::new();

    for row in &data_rows {
        let row = *row;
        let solo_text = format!("{header_block}\n{row}");

        if sizer.size(&solo_text) > target {
            // Oversized single row: not even a standalone chunk (header + this one row) fits
            // under the target. Flush whatever is pending first (so those rows aren't lost or
            // reordered), then fall back to chunk_code's long-line split for this row alone.
            flush_table_batch(
                &header_block,
                &mut pending,
                &mut chunks,
                &mut seq_in_block,
                block_seq,
            );
            // `chunk_code` computes spans relative to its input (`row`); rebase them onto
            // the block so they keep the exact-slice contract (`row` borrows from
            // `markdown`, so pointer arithmetic gives its byte offset).
            let row_off = row.as_ptr() as usize - markdown.as_ptr() as usize;
            let row_chunks = chunk_code(resource_id, row, config, block_seq)?;
            for mut rc in row_chunks {
                rc.span = Span::new(rc.span.start + row_off, rc.span.end + row_off);
                rc.block_seq = block_seq;
                rc.seq_in_block = seq_in_block;
                seq_in_block += 1;
                chunks.push(rc);
            }
            continue;
        }

        // Row fits alone; try greedily packing it into the current batch.
        let mut candidate = pending.clone();
        candidate.push(row);
        let candidate_text = format!("{header_block}\n{}", candidate.join("\n"));
        if sizer.size(&candidate_text) <= target {
            pending.push(row);
        } else {
            flush_table_batch(
                &header_block,
                &mut pending,
                &mut chunks,
                &mut seq_in_block,
                block_seq,
            );
            pending.push(row);
        }
    }
    flush_table_batch(
        &header_block,
        &mut pending,
        &mut chunks,
        &mut seq_in_block,
        block_seq,
    );

    // Ids depend on `block_seq`/`seq_in_block`, both final at this point.
    finalize_ids(resource_id, &mut chunks);

    Ok(chunks)
}

/// Flush the pending batch of table data rows into one chunk, re-emitting the header and
/// separator rows so the chunk is a standalone, valid Markdown table.
fn flush_table_batch(
    header_block: &str,
    pending: &mut Vec<&str>,
    chunks: &mut Vec<ChunkOutput>,
    seq_in_block: &mut u32,
    block_seq: u32,
) {
    if pending.is_empty() {
        return;
    }
    let text = format!("{header_block}\n{}", pending.join("\n"));
    chunks.push(ChunkOutput::placeholder(
        text,
        Span::new(0, 0), // reconstructed (header re-emitted per chunk); span isn't meaningful
        vec![],
        block_seq,
        *seq_in_block,
    ));
    *seq_in_block += 1;
    pending.clear();
}

/// Recognize a Markdown table separator row, e.g. `|---|---|` or `| :--- | ---: |`.
fn is_table_separator_row(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let inner = trimmed.trim_matches('|');
    if inner.is_empty() {
        return false;
    }
    inner.split('|').all(|cell| {
        let cell = cell.trim();
        !cell.is_empty()
            && cell.contains('-')
            && cell
                .chars()
                .all(|c| c == '-' || c == ':' || c.is_whitespace())
    })
}
