//! `chunk_blocks` dispatch tests.

use crate::chunker::formats::FORMATS;
use crate::chunker::{chunk_blocks, CharSizer, ChunkerConfig};
use crate::ids::resource_id;

// ---------------------------------------------------------------------------
// Fix 4: code preset routes prose-shaped blocks through code chunker
// ---------------------------------------------------------------------------

#[test]
fn code_preset_routes_text_block_through_code_chunker() {
    // A Text block fed to chunk_blocks with preset="code" must go
    // through the code (line-packer) path, not the prose (MarkdownSplitter) path.
    // We verify this by checking that the chunks are produced (no panic) and
    // that their spans are valid byte ranges.
    let block = crate::block::Block {
        seq: 0,
        kind: crate::block::BlockKind::Text,
        text: "fn hello() {\n    println!(\"hi\");\n}".to_string(),
        location: None,
    };
    let doc_id = resource_id("file:///test.rs", "abc");
    let cfg = ChunkerConfig::code();
    let chunks = chunk_blocks(&doc_id, &[block], &cfg, &CharSizer).unwrap();
    assert!(
        !chunks.is_empty(),
        "code preset + Text should produce chunks"
    );
    for c in &chunks {
        assert!(c.span.start <= c.span.end, "span start <= end");
    }
}

// ---------------------------------------------------------------------------
// FormatChunker registry: claims precedence and sanity (#213 Stage 3)
// ---------------------------------------------------------------------------

#[test]
fn registry_order_and_names() {
    let names: Vec<&str> = FORMATS.iter().map(|f| f.name()).collect();
    assert_eq!(
        names,
        vec!["messages", "code", "prose", "table", "passthrough"],
        "registry order encodes claim precedence; Code must precede Prose"
    );
}

#[test]
fn claims_precedence_heading_text_routes_by_preset() {
    // Heading/Text blocks are claimed by Code under the code preset, and by Prose
    // otherwise — Code's precedence over Prose in FORMATS encodes this routing.
    let heading = crate::block::Block {
        seq: 0,
        kind: crate::block::BlockKind::Heading { level: 1 },
        text: "Title".to_string(),
        location: None,
    };
    let text = crate::block::Block {
        seq: 1,
        kind: crate::block::BlockKind::Text,
        text: "Body".to_string(),
        location: None,
    };
    let code_cfg = ChunkerConfig::code();
    let prose_cfg = ChunkerConfig::prose();

    for block in [&heading, &text] {
        let claimed = FORMATS
            .iter()
            .find(|f| f.claims(block, &code_cfg))
            .expect("some format must claim this block under the code preset");
        assert_eq!(
            claimed.name(),
            "code",
            "{:?} under code preset should be claimed by Code first",
            block.kind
        );

        let claimed = FORMATS
            .iter()
            .find(|f| f.claims(block, &prose_cfg))
            .expect("some format must claim this block under the prose preset");
        assert_eq!(
            claimed.name(),
            "prose",
            "{:?} under prose preset should be claimed by Prose",
            block.kind
        );
    }
}

#[test]
fn claims_message_and_segment_always_go_to_messages() {
    // Message/Segment blocks are claimed by Messages regardless of preset.
    let msg = crate::block::Block {
        seq: 0,
        kind: crate::block::BlockKind::Message {
            sender: "Alice".to_string(),
            timestamp: None,
            message_id: None,
            reply_to: None,
        },
        text: "hi".to_string(),
        location: None,
    };
    let seg = crate::block::Block {
        seq: 1,
        kind: crate::block::BlockKind::Segment {
            speaker: None,
            start_ms: 0,
            end_ms: 100,
        },
        text: "hi".to_string(),
        location: None,
    };
    for cfg in [
        ChunkerConfig::prose(),
        ChunkerConfig::code(),
        ChunkerConfig::messages(),
    ] {
        for block in [&msg, &seg] {
            let claimed = FORMATS
                .iter()
                .find(|f| f.claims(block, &cfg))
                .unwrap_or_else(|| {
                    panic!(
                        "{:?} must be claimed under preset {}",
                        block.kind, cfg.preset
                    )
                });
            assert_eq!(claimed.name(), "messages");
        }
    }
}

#[test]
fn claims_table_block_goes_to_table() {
    let table = crate::block::Block {
        seq: 0,
        kind: crate::block::BlockKind::Table {
            headers: vec!["A".to_string()],
            rows: 1,
        },
        text: "| A |\n|---|\n| 1 |".to_string(),
        location: None,
    };
    let claimed = FORMATS
        .iter()
        .find(|f| f.claims(&table, &ChunkerConfig::prose()))
        .expect("Table block must be claimed by some format");
    assert_eq!(claimed.name(), "table");
}

#[test]
fn claims_passthrough_kinds() {
    // Reference/Attachment/Frontmatter/Image are claimed by Passthrough — NOT a
    // catch-all, but an explicit list (see Passthrough::claims doc comment).
    let blocks = vec![
        crate::block::Block {
            seq: 0,
            kind: crate::block::BlockKind::Reference {
                target: "http://example.com".to_string(),
                label: None,
                ref_type: None,
            },
            text: "ref".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 1,
            kind: crate::block::BlockKind::Attachment {
                filename: "f.pdf".to_string(),
                mime: None,
                size_bytes: None,
            },
            text: "att".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 2,
            kind: crate::block::BlockKind::Frontmatter {
                format: "yaml".to_string(),
            },
            text: "fm".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 3,
            kind: crate::block::BlockKind::Image {
                alt: None,
                src: None,
            },
            text: "img".to_string(),
            location: None,
        },
    ];
    let cfg = ChunkerConfig::prose();
    for b in &blocks {
        let claimed = FORMATS
            .iter()
            .find(|f| f.claims(b, &cfg))
            .unwrap_or_else(|| panic!("block kind {:?} must be claimed by some format", b.kind));
        assert_eq!(
            claimed.name(),
            "passthrough",
            "block kind {:?} should be claimed by Passthrough",
            b.kind
        );
    }
}

// ---------------------------------------------------------------------------
// chunk_blocks output-equivalence: message chunks first, then per-block chunks in doc
// order, unaffected by the FormatChunker-registry rewrite (#213 Stage 3).
// ---------------------------------------------------------------------------

#[test]
fn chunk_blocks_mixed_kinds_message_first_then_per_block_in_doc_order() {
    // A mixed-format doc: text, code, table, reference, then two messages. Message-window
    // chunks (document-scoped) must come first in the output, followed by the per-block
    // chunks in doc order — this is today's two-pass dispatch behavior and must stay green
    // across the rewrite from a hand-written match to the FormatChunker registry.
    let blocks = vec![
        crate::block::Block {
            seq: 0,
            kind: crate::block::BlockKind::Text,
            text: "Intro text.".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 1,
            kind: crate::block::BlockKind::Code { language: None },
            text: "fn f() {}".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 2,
            kind: crate::block::BlockKind::Table {
                headers: vec!["A".to_string()],
                rows: 1,
            },
            text: "| A |\n|---|\n| 1 |".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 3,
            kind: crate::block::BlockKind::Reference {
                target: "http://example.com".to_string(),
                label: None,
                ref_type: None,
            },
            text: "See reference".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 4,
            kind: crate::block::BlockKind::Message {
                sender: "Alice".to_string(),
                timestamp: None,
                message_id: None,
                reply_to: None,
            },
            text: "Hello".to_string(),
            location: None,
        },
        crate::block::Block {
            seq: 5,
            kind: crate::block::BlockKind::Message {
                sender: "Bob".to_string(),
                timestamp: None,
                message_id: None,
                reply_to: None,
            },
            text: "Hi".to_string(),
            location: None,
        },
    ];
    let cfg = ChunkerConfig::prose();
    let chunks = chunk_blocks("resource-mixed", &blocks, &cfg, &CharSizer).unwrap();

    // Message-window chunk(s) come first.
    let message_count = chunks
        .iter()
        .filter(|c| !c.window_block_seqs.is_empty())
        .count();
    assert_eq!(
        message_count, 1,
        "two short messages should fit in one window; chunks: {chunks:#?}"
    );
    assert_eq!(
        chunks[0].window_block_seqs,
        vec![4, 5],
        "message window should cover both message blocks; chunks: {chunks:#?}"
    );

    // Remaining chunks are per-block, in doc order: text(0), code(1), table(2), reference(3).
    let per_block = &chunks[message_count..];
    assert_eq!(
        per_block.len(),
        4,
        "one chunk per non-message block; chunks: {chunks:#?}"
    );
    let expected = [(0u32, "text"), (1, "code"), (2, "table"), (3, "reference")];
    for (chunk, (seq, kind)) in per_block.iter().zip(expected.iter()) {
        assert_eq!(
            chunk.block_seq, *seq,
            "block_seq mismatch; chunks: {chunks:#?}"
        );
        assert_eq!(
            chunk.seq_in_block, 0,
            "single-chunk block should have seq_in_block 0"
        );
        assert_eq!(
            chunk.block_kind.as_deref(),
            Some(*kind),
            "block_kind mismatch for block_seq {seq}"
        );
    }
}
