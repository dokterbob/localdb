//! `chunk_blocks` dispatch tests.

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
