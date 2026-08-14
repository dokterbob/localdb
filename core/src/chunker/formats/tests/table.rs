//! Table chunker tests.

use crate::chunker::formats::code::chunk_code;
use crate::chunker::formats::table::chunk_table;
use crate::chunker::{CharSizer, ChunkerConfig};
use crate::ids::resource_id;

// ---------------------------------------------------------------------------
// Table chunker tests
// ---------------------------------------------------------------------------

#[test]
fn table_small_single_chunk_unchanged() {
    let md = "| Name | Age |\n|---|---|\n| Alice | 30 |\n| Bob | 25 |";
    let doc_id = resource_id("file:///table.md", "abc");
    let cfg = ChunkerConfig::prose();
    let chunks = chunk_table(&doc_id, md, &cfg, &CharSizer, 0).unwrap();
    assert_eq!(chunks.len(), 1, "small table should fit in a single chunk");
    assert!(chunks[0].text.contains("| Name | Age |"));
    assert!(chunks[0].text.contains("|---|---|"));
    assert!(chunks[0].text.contains("| Alice | 30 |"));
    assert!(chunks[0].text.contains("| Bob | 25 |"));
}

#[test]
fn table_header_only_block_emits_one_chunk_with_header() {
    // A table block with a header + separator row but NO data rows must still
    // produce a chunk (the header content must not silently vanish from the index).
    let md = "| Name | Age |\n|---|---|";
    let doc_id = resource_id("file:///table_header_only.md", "abc");
    let cfg = ChunkerConfig::prose();
    let chunks = chunk_table(&doc_id, md, &cfg, &CharSizer, 2).unwrap();
    assert_eq!(
        chunks.len(),
        1,
        "header-only table must produce exactly one chunk, got {}",
        chunks.len()
    );
    assert_eq!(chunks[0].text, "| Name | Age |\n|---|---|");
    assert_eq!(chunks[0].block_seq, 2);
    assert_eq!(chunks[0].seq_in_block, 0);
    assert!(!chunks[0].id.is_empty(), "chunk must have a valid id");
}

#[test]
fn table_multi_chunk_split_preserves_header() {
    // header_block = "| A | B |\n|---|---|" = 19 chars; each row "| 1 | 2 |" = 9 chars.
    // target=40 packs exactly 2 rows per chunk (19+1+9+1+9=39<=40; a 3rd row would be 49>40).
    let mut md = String::from("| A | B |\n|---|---|\n");
    let rows: Vec<String> = (0..10).map(|i| format!("| {i} | {i} |")).collect();
    md.push_str(&rows.join("\n"));
    let doc_id = resource_id("file:///table_big.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(40),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_table(&doc_id, &md, &cfg, &CharSizer, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "10 rows under a tight target must split into multiple chunks, got {}",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            c.text.contains("| A | B |") && c.text.contains("|---|---|"),
            "every chunk must re-emit the header/separator; got: {:?}",
            c.text
        );
    }
    // Every original row must appear in exactly one chunk, in order.
    let all_rows_text: String = chunks.iter().map(|c| c.text.as_str()).collect();
    for row in &rows {
        assert!(
            all_rows_text.contains(row.as_str()),
            "row {row:?} must appear in the chunked output"
        );
    }
}

#[test]
fn table_oversized_single_row_falls_back_to_code_chunker_split() {
    // A single data row so large that even header+separator+row alone exceeds the
    // target must be split via chunk_code's long-line logic, not silently over-grown.
    let huge_cell = "x".repeat(1000);
    let md = format!("| A |\n|---|\n| {huge_cell} |");
    let doc_id = resource_id("file:///table_oversized.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(40),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_table(&doc_id, &md, &cfg, &CharSizer, 0).unwrap();
    assert!(
        chunks.len() > 1,
        "oversized single row must be split into multiple bounded chunks, got {}",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            c.text.chars().count() <= 2 * cfg.resolved_target_tokens(),
            "fallback chunk must stay bounded: {} chars",
            c.text.chars().count()
        );
        // The fallback's spans are rebased from row-relative to block-relative
        // coordinates, so they must keep the exact-slice contract — a plausible
        // span pointing at the wrong text would be worse than a placeholder.
        assert_eq!(
            &md[c.span.start..c.span.end],
            c.text,
            "oversized-row fallback span must slice the block to exactly the chunk text"
        );
    }
}

#[test]
fn table_malformed_no_pipes_falls_back_to_code_chunker() {
    // No recognizable header/separator row at all (no `|` characters anywhere) — must
    // fall back to exactly the previous (code chunker) behavior, not panic or guess.
    let md = "Name Age\nAlice 30\nBob 25\n";
    let doc_id = resource_id("file:///table_malformed.md", "abc");
    let cfg = ChunkerConfig::code();
    let table_chunks = chunk_table(&doc_id, md, &cfg, &CharSizer, 0).unwrap();
    let code_chunks = chunk_code(&doc_id, md, &cfg, 0).unwrap();
    assert_eq!(
        table_chunks, code_chunks,
        "malformed table text must fall back to exactly chunk_code's output"
    );
}

#[test]
fn table_malformed_missing_dash_separator_falls_back_to_code_chunker() {
    // Header row has pipes, but the second line isn't a `---`-style separator —
    // must be treated as malformed and fall back, not mis-parsed as data.
    let md = "| A | B |\n| 1 | 2 |\n| 3 | 4 |";
    let doc_id = resource_id("file:///table_malformed2.md", "abc");
    let cfg = ChunkerConfig::code();
    let table_chunks = chunk_table(&doc_id, md, &cfg, &CharSizer, 0).unwrap();
    let code_chunks = chunk_code(&doc_id, md, &cfg, 0).unwrap();
    assert_eq!(
        table_chunks, code_chunks,
        "missing dash-separator row must fall back to exactly chunk_code's output"
    );
}

#[test]
fn table_token_target_boundary_packs_up_to_exact_target() {
    // header_block = "| A |\n|---|" = 11 chars; each row "| 1 |" = 5 chars.
    // 2 rows: 11+1+5+1+5 = 23 (fits exactly at target=23). A 3rd row would be 29 (over).
    let md = "| A |\n|---|\n| 1 |\n| 2 |\n| 3 |";
    let doc_id = resource_id("file:///table_boundary.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(23),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_table(&doc_id, md, &cfg, &CharSizer, 0).unwrap();
    assert_eq!(
        chunks.len(),
        2,
        "rows 1+2 should pack exactly at the boundary, row 3 starts a new chunk"
    );
    assert!(chunks[0].text.contains("| 1 |") && chunks[0].text.contains("| 2 |"));
    assert!(!chunks[0].text.contains("| 3 |"));
    assert!(chunks[1].text.contains("| 3 |"));
}

#[test]
fn table_chunk_ids_are_content_addressed_and_unique() {
    let mut md = String::from("| A | B |\n|---|---|\n");
    let rows: Vec<String> = (0..10).map(|i| format!("| {i} | {i} |")).collect();
    md.push_str(&rows.join("\n"));
    let doc_id = resource_id("file:///table_ids.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(40),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let run1 = chunk_table(&doc_id, &md, &cfg, &CharSizer, 3).unwrap();
    let run2 = chunk_table(&doc_id, &md, &cfg, &CharSizer, 3).unwrap();
    assert_eq!(run1.len(), run2.len());
    for (c1, c2) in run1.iter().zip(run2.iter()) {
        assert_eq!(c1.id, c2.id, "table chunk ids must be deterministic");
    }
    let unique_ids: std::collections::HashSet<&str> = run1.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        unique_ids.len(),
        run1.len(),
        "table chunk ids must be unique"
    );
    for c in &run1 {
        assert_eq!(
            c.block_seq, 3,
            "block_seq must be threaded through to table chunks"
        );
    }
}
