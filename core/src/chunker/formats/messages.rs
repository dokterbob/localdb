//! Messages chunker: sliding-window chunker over `Message`/`Segment` blocks.

use crate::block::Block;
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
/// over the FULL document (see [`GroupScope::Document`]), since message windows span
/// multiple blocks. `chunk_messages` does its own filtering/stamping/finalization
/// internally, so the claimed-subset `blocks` argument is ignored in favor of
/// `ctx.blocks` — exactly as today's dispatch calls it.
pub(in crate::chunker) struct Messages;

impl FormatChunker for Messages {
    fn name(&self) -> &'static str {
        "messages"
    }

    fn scope(&self) -> GroupScope {
        GroupScope::Document
    }

    fn claims(&self, block: &Block, _config: &ChunkerConfig) -> bool {
        matches!(
            block.kind,
            crate::block::BlockKind::Message { .. } | crate::block::BlockKind::Segment { .. }
        )
    }

    fn chunk(&self, ctx: &ChunkContext<'_>, _blocks: &[&Block]) -> Result<Vec<ChunkOutput>, Error> {
        chunk_messages(ctx.resource_id, ctx.blocks, ctx.config, ctx.sizer)
    }
}
