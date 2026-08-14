//! Messages chunker tests: sliding windows, end-shrink coverage, and id finalization.

use super::common::{assert_no_mid_word_splits, WordSizer};
use crate::chunker::{chunk_messages, CharSizer, ChunkerConfig};

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
        kind: crate::block::BlockKind::Text,
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
    // Heading + Text + 3 Message + Text + 1 Message
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
            kind: crate::block::BlockKind::Text,
            text: "Intro paragraph.".to_string(),
            location: None,
        },
        msg_block(2, "Alice", "2026-01-01T10:00:00Z", "First message"),
        msg_block(3, "Bob", "2026-01-01T10:01:00Z", "Second message"),
        msg_block(4, "Alice", "2026-01-01T10:02:00Z", "Third message"),
        crate::block::Block {
            seq: 5,
            kind: crate::block::BlockKind::Text,
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
fn messages_long_single_message_no_mid_word_splits() {
    // An oversize single message turn of ordinary space-separated prose (#191):
    // `chunk_messages`'s "split a too-long single turn" branch delegates to
    // `chunk_prose` (see the module doc comment on `chunk_messages`), so the
    // mid-word-split fix must flow through this path too. `pc.span` is threaded
    // through to the final `ChunkOutput.span` unchanged (relative to the raw,
    // unprefixed `block.text`), so `assert_no_mid_word_splits` can check it directly
    // against `long_text`.
    let sentence =
        "The quick brown fox jumps over the lazy dog and runs swiftly through the forest. ";
    let mut long_text = String::new();
    while long_text.len() < 2200 {
        long_text.push_str(sentence);
    }
    assert!(
        !long_text.contains('\n'),
        "message body must be a single line"
    );
    let blocks = vec![msg_block(0, "Alice", "2026-01-01T10:00:00Z", &long_text)];
    let cfg = ChunkerConfig {
        preset: "messages".to_string(),
        target_tokens: Some(50), // small budget to force splitting
        overlap_tokens: Some(0),
        window_turns: Some(6),
        stride_turns: Some(3),
    };
    let chunks = chunk_messages("resource-mid-word", &blocks, &cfg, &WordSizer).unwrap();
    assert!(
        chunks.len() > 1,
        "long single message should split into multiple sub-chunks, got {}",
        chunks.len()
    );
    assert_no_mid_word_splits(&long_text, &chunks);
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

#[test]
fn messages_stride_advances_by_covered_turns_when_window_shrunk() {
    // 10 turns, stride=3, budget so tight each turn exceeds it on its own.
    // The end-shrink fix already handles reducing actual_end to window_start+1.
    // The stride fix ensures we advance by 1 (turns_covered=1), not by stride=3,
    // so every turn appears as a window_start.
    // Each turn text is "[U]: 1234567890" (16 chars), budget = 5 chars.
    let turns: Vec<_> = (0..10u32)
        .map(|i| msg_block(i, "U", "2026-01-01", "1234567890"))
        .collect();
    let cfg = ChunkerConfig {
        preset: "messages".to_string(),
        target_tokens: Some(5), // smaller than a single "[U]: 1234567890" turn
        overlap_tokens: Some(0),
        window_turns: Some(3),
        stride_turns: Some(3),
    };
    let chunks = chunk_messages("resource-stride", &turns, &cfg, &CharSizer).unwrap();
    // Every turn must appear in at least one window.
    let covered_seqs: std::collections::HashSet<u32> = chunks
        .iter()
        .flat_map(|c| c.window_block_seqs.iter().copied())
        .collect();
    for i in 0u32..10 {
        assert!(
            covered_seqs.contains(&i),
            "turn {i} must appear in at least one window; covered: {covered_seqs:?}"
        );
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

// ---------------------------------------------------------------------------
// Chunk id finalization: message-window chunks get ids from FINAL membership
// ---------------------------------------------------------------------------

#[test]
fn message_window_ids_reflect_final_membership_after_fixup() {
    // Two messages, EACH long enough to exceed the token budget on its own, force
    // BOTH through chunk_messages' "split a too-long single turn" branch. Each split
    // pushes its sub-chunks with a branch-local index (0, 1, 2, ...) that only matches
    // the chunk's true global position for the FIRST message; the second message's
    // sub-chunks are appended after the first message's, so their local index no
    // longer matches their final position until the end-of-sequence "Fix seq_in_block"
    // pass runs. This is exactly the case the finalize-after-fixup design must get
    // right: ids must be computed from final (fixed-up) seq_in_block, not the
    // branch-local one.
    let long_text = "word ".repeat(200);
    let blocks = vec![
        msg_block(0, "Alice", "2026-01-01T10:00:00Z", &long_text),
        msg_block(1, "Bob", "2026-01-01T10:01:00Z", &long_text),
    ];
    let cfg = ChunkerConfig {
        preset: "messages".to_string(),
        target_tokens: Some(50),
        overlap_tokens: Some(0),
        window_turns: Some(6),
        stride_turns: Some(3),
    };

    let run1 = chunk_messages("resource-fixup", &blocks, &cfg, &WordSizer).unwrap();
    let run2 = chunk_messages("resource-fixup", &blocks, &cfg, &WordSizer).unwrap();

    assert!(
        run1.len() > 2,
        "expected both long messages to split into multiple sub-chunks each, got {}",
        run1.len()
    );

    // (b) stable across two identical chunker runs.
    assert_eq!(run1.len(), run2.len(), "chunk count must be stable");
    for (c1, c2) in run1.iter().zip(run2.iter()) {
        assert_eq!(
            c1.id, c2.id,
            "chunk ids must be stable across identical runs"
        );
    }

    // (a) unique.
    let unique_ids: std::collections::HashSet<&str> = run1.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        unique_ids.len(),
        run1.len(),
        "all message-window chunk ids must be unique"
    );

    // (c) derived from FINAL membership: seq_in_block must already be the fixed-up,
    // sequential index, and each id must equal the formula applied to that final value.
    for (i, c) in run1.iter().enumerate() {
        assert_eq!(
            c.seq_in_block, i as u32,
            "seq_in_block must be the final, fixed-up index"
        );
        let expected = crate::ids::chunk_id("resource-fixup", c.block_seq, &c.text, c.seq_in_block);
        assert_eq!(
            c.id, expected,
            "chunk id must equal ids::chunk_id(resource_id, block_seq, text, seq_in_block) \
             computed from the chunk's FINAL block_seq/seq_in_block"
        );
    }
}
