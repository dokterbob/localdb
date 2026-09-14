//! Messages chunker: sliding-window chunker over `Message`/`Segment` blocks.

use crate::block::{Block, BlockKind};
use crate::chunker::output::{finalize_ids, ChunkOutput};
use crate::chunker::sizers::ChunkSizer;
use crate::chunker::ChunkerConfig;
use crate::ids::ContentId;
use crate::Error;

use super::prose::chunk_prose;
use super::{ChunkContext, FormatChunker, GroupScope};

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

/// Whether `kind` is a message-window "turn" (`Message` or `Segment`).
fn is_turn_block(kind: &BlockKind) -> bool {
    matches!(kind, BlockKind::Message { .. } | BlockKind::Segment { .. })
}

/// Format the turn-prefix label for a `Message`/`Segment` block's kind.
fn turn_prefix(kind: &BlockKind) -> String {
    match kind {
        BlockKind::Message {
            sender, timestamp, ..
        } => format_message_prefix(sender, timestamp.as_deref()),
        BlockKind::Segment {
            speaker,
            start_ms,
            end_ms,
        } => format_segment_prefix(speaker.as_deref(), *start_ms, *end_ms),
        _ => unreachable!(),
    }
}

/// Split a single turn that alone exceeds `max_tokens` using prose-chunker logic, prepending
/// the sender/speaker prefix to each sub-chunk. Extracted from `chunk_messages`'s oversized-
/// single-turn branch — pure code motion, no logic change.
fn chunk_oversized_turn(
    resource_id: &str,
    block: &Block,
    config: &ChunkerConfig,
    sizer: &dyn ChunkSizer,
) -> Result<Vec<ChunkOutput>, Error> {
    // Split the raw message body (without prefix) using prose chunker,
    // then prepend the sender/speaker context to each sub-chunk.
    let prefix = turn_prefix(&block.kind);
    let prose_chunks = chunk_prose(resource_id, &block.text, config, sizer, block.seq)?;
    let first_seq = block.seq;
    let kind_str = block.kind.kind_str().to_string();
    let mut out = Vec::with_capacity(prose_chunks.len());
    for (i, pc) in prose_chunks.into_iter().enumerate() {
        let prefixed_text = format!("{prefix}{}", pc.text);
        // Id is a placeholder here — sub-chunk position within the FULL message-chunk
        // sequence (across all windows) isn't known until the "Fix seq_in_block" pass
        // below runs; `finalize_ids` computes the real id afterward.
        out.push(ChunkOutput {
            id: ContentId::new(),
            text: prefixed_text,
            span: pc.span,
            heading_path: vec![],
            block_seq: first_seq,
            seq_in_block: i as u32,
            window_block_seqs: vec![first_seq],
            block_kind: Some(kind_str.clone()),
        });
    }
    Ok(out)
}

/// Shrink `window_end_excl` down to the largest window end (> `window_start`) whose joined
/// turn text fits within `max_tokens`, shrinking from the end so every turn appears in at
/// least one window (shrinking from the front would silently skip leading turns). Returns
/// `window_end_excl` unchanged if it already fits. Extracted from `chunk_messages`'s window
/// loop — pure code motion, no logic change.
fn fit_window_end(
    turn_texts: &[String],
    window_start: usize,
    window_end_excl: usize,
    sizer: &dyn ChunkSizer,
    max_tokens: usize,
) -> usize {
    let candidate_text: String = turn_texts[window_start..window_end_excl].join("\n\n");
    if sizer.size(&candidate_text) <= max_tokens {
        return window_end_excl;
    }

    let mut actual_end = window_end_excl;
    while actual_end > window_start + 1 {
        let candidate: String = turn_texts[window_start..actual_end].join("\n\n");
        if sizer.size(&candidate) <= max_tokens {
            break;
        }
        actual_end -= 1;
    }
    actual_end
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
    let max_tokens = config.resolved_target_tokens();
    let window_turns = config.resolved_window_turns();
    let stride_turns = config.resolved_stride_turns();
    let stride_turns = stride_turns.max(1); // prevent infinite loop

    // Collect only Message/Segment blocks, in order.
    let turns: Vec<&crate::block::Block> = blocks
        .iter()
        .filter(|b| !b.text.is_empty() && is_turn_block(&b.kind))
        .collect();

    if turns.is_empty() {
        return Ok(vec![]);
    }

    // Build prefixed text for each turn.
    let turn_texts: Vec<String> = turns
        .iter()
        .map(|b| format!("{}{}", turn_prefix(&b.kind), b.text))
        .collect();

    let mut out: Vec<ChunkOutput> = Vec::new();
    let n = turns.len();
    let mut window_start = 0usize;

    while window_start < n {
        let window_end_excl = (window_start + window_turns).min(n);

        // Determine how many turns fit within the token budget.
        let actual_end = fit_window_end(
            &turn_texts,
            window_start,
            window_end_excl,
            sizer,
            max_tokens,
        );

        let window_seqs: Vec<u32> = turns[window_start..actual_end]
            .iter()
            .map(|b| b.seq)
            .collect();

        // If even a single turn is too long, split it with prose chunker logic.
        if actual_end == window_start + 1 && sizer.size(&turn_texts[window_start]) > max_tokens {
            let block = turns[window_start];
            out.extend(chunk_oversized_turn(resource_id, block, config, sizer)?);
        } else {
            let window_text: String = turn_texts[window_start..actual_end].join("\n\n");
            let first_seq = turns[window_start].seq;
            let kind_str = turns[window_start].kind.kind_str().to_string();
            // Id is a placeholder — see note above; `finalize_ids` runs after fix-up.
            out.push(ChunkOutput {
                id: ContentId::new(),
                text: window_text,
                span: crate::types::Span::new(0, 0), // not meaningful for multi-block windows
                heading_path: vec![],
                block_seq: first_seq,
                seq_in_block: out.len() as u32, // index among message chunks
                window_block_seqs: window_seqs,
                block_kind: Some(kind_str),
            });
        }

        let turns_covered = actual_end - window_start;
        if actual_end < window_end_excl {
            // Window was shrunk — advance by what we covered to avoid skipping turns.
            window_start += turns_covered;
        } else {
            // Normal window — advance by stride.
            window_start += stride_turns;
        }
    }

    // Fix seq_in_block: should be the chunk's index within all message chunks. This is the
    // end-of-sequence fix-up pass referenced in specs/04-search-pipeline.md §3 — window chunk
    // ids MUST be computed after this runs, since a chunk's final `seq_in_block` (and thus its
    // id) is only settled once every window in the sequence has been produced.
    for (i, c) in out.iter_mut().enumerate() {
        c.seq_in_block = i as u32;
    }

    // Now that block_seq/seq_in_block are final for every message-window chunk, compute ids.
    finalize_ids(resource_id, &mut out);

    Ok(out)
}

// ---------------------------------------------------------------------------
// FormatChunker impl
// ---------------------------------------------------------------------------

/// `FormatChunker` for `Message`/`Segment` blocks. Document-scoped: `chunk` is invoked once
/// with all the document's claimed turn blocks (see [`GroupScope::Document`]), since
/// message windows span multiple blocks. `chunk_messages` does its own filtering/stamping/
/// finalization internally over the full document, so the claimed-subset `blocks` argument
/// is ignored in favor of `ctx.blocks`.
pub(in crate::chunker) struct Messages;

impl FormatChunker for Messages {
    fn name(&self) -> &'static str {
        "messages"
    }

    fn scope(&self) -> GroupScope {
        GroupScope::Document
    }

    fn claims(&self, block: &Block, _config: &ChunkerConfig) -> bool {
        is_turn_block(&block.kind)
    }

    fn chunk(&self, ctx: &ChunkContext<'_>, _blocks: &[&Block]) -> Result<Vec<ChunkOutput>, Error> {
        chunk_messages(ctx.resource_id, ctx.blocks, ctx.config, ctx.sizer)
    }
}
