//! Code chunker tests, including lockfile/minified-file hang regression tests.

use crate::chunker::formats::code::chunk_code;
use crate::chunker::{preset_for, ChunkerConfig};
use crate::ids::resource_id;

// ---------------------------------------------------------------------------
// Code chunker tests (interim line packer)
// ---------------------------------------------------------------------------

#[test]
fn code_chunk_empty_returns_empty() {
    let doc_id = resource_id("file:///lib.rs", "abc");
    let cfg = ChunkerConfig::code();
    let chunks = chunk_code(&doc_id, "", &cfg, 0).unwrap();
    assert!(chunks.is_empty());
}

#[test]
fn code_chunk_single_block() {
    let full_text = "fn hello() {\n    println!(\"hi\");\n}";
    let doc_id = resource_id("file:///lib.rs", "abc");
    let cfg = ChunkerConfig::code();

    let chunks = chunk_code(&doc_id, full_text, &cfg, 0).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text, full_text);
}

#[test]
fn code_chunk_large_splits() {
    let line = "let x = some_function_with_long_name(arg1, arg2, arg3);\n";
    let full_text = line.repeat(100); // ~5600 chars
    let doc_id = resource_id("file:///lib.rs", "hash");
    let cfg = ChunkerConfig::code();

    let chunks = chunk_code(&doc_id, &full_text, &cfg, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "large code file should produce multiple chunks"
    );
}

#[test]
fn code_chunk_spans_round_trip() {
    let line = "let x = 1;\n";
    let full_text = line.repeat(200);
    let doc_id = resource_id("file:///lib.rs", "hash");
    let cfg = ChunkerConfig::code();
    let chunks = chunk_code(&doc_id, &full_text, &cfg, 0).unwrap();
    for c in &chunks {
        assert!(
            c.span.start <= c.span.end,
            "span start must be <= span end (sanity check)"
        );
        assert_eq!(
            &full_text[c.span.start..c.span.end],
            c.text,
            "span slice must exactly equal chunk text"
        );
    }
}

#[test]
fn chunk_blocks_multibyte_code_preset_does_not_panic() {
    let unit = "日本語テキスト: これはテストです。 ";
    let text = unit.repeat(200);
    let doc_id = "doc-multibyte-code";
    let result = chunk_code(doc_id, &text, &ChunkerConfig::code(), 0);
    assert!(
        result.is_ok(),
        "code chunking multi-byte text should not panic: {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------------
// Regression tests: hang-fix for code/JSON/lockfiles (only-index-supported-files)
// ---------------------------------------------------------------------------

/// Regression: minified JSON (one very long line) must not hang and must produce
/// bounded chunks. Before the fix, `chunk_prose` was called on structureless JSON,
/// causing super-linear cost and a multi-minute hang.
#[test]
fn regression_minified_json_does_not_hang() {
    let unit = r#"{"key":"value","#;
    // 100_000 chars ≈ 6250 repetitions of the 16-char unit
    let reps = 100_000 / unit.len();
    let content = unit.repeat(reps);
    let doc_id = "doc-minified-json";
    let cfg = ChunkerConfig::code(); // target = 3000 chars
    let chunks = chunk_code(doc_id, &content, &cfg, 0).unwrap();
    // Must produce more than one chunk (content >> target).
    assert!(
        chunks.len() > 1,
        "minified JSON must split into multiple chunks, got {}",
        chunks.len()
    );
    // Every chunk must be within 2× the char target.
    let target = cfg.resolved_target_tokens();
    for c in &chunks {
        let char_count = c.text.chars().count();
        assert!(
            char_count <= 2 * target,
            "chunk exceeds 2× target ({} chars, target {})",
            char_count,
            target
        );
    }
}

/// Regression: a Rust source file must be routed to the code chunker, not prose.
/// Before the fix, `preset_for` did not exist and all files defaulted to prose.
#[test]
fn regression_code_file_uses_line_chunker_not_prose() {
    assert_eq!(
        preset_for(Some("main.rs"), None),
        "code",
        "main.rs must route to the code chunker"
    );
}

/// Regression: a Markdown README must still use the prose chunker.
#[test]
fn regression_prose_file_uses_prose_chunker() {
    assert_eq!(
        preset_for(Some("README.md"), None),
        "prose",
        "README.md must route to the prose chunker"
    );
}

/// Regression: Cargo.lock (lockfile, no recognized extension) must route to code.
/// Before the fix, Cargo.lock would fall through to prose and hang on its
/// long structureless sections.
#[test]
fn regression_cargo_lock_uses_line_chunker() {
    assert_eq!(
        preset_for(Some("Cargo.lock"), None),
        "code",
        "Cargo.lock must route to the code chunker"
    );
}

#[test]
fn preset_for_spreadsheet_exts_is_code() {
    assert_eq!(preset_for(Some("sheet.xlsx"), None), "code");
    assert_eq!(preset_for(Some("sheet.xls"), None), "code");
    // Case-insensitive
    assert_eq!(preset_for(Some("SHEET.XLSX"), None), "code");
}

#[test]
fn preset_for_docx_pptx_is_prose() {
    // DOCX and PPTX are prose documents, not tabular/code data.
    assert_eq!(preset_for(Some("report.docx"), None), "prose");
    assert_eq!(preset_for(Some("slides.pptx"), None), "prose");
}

#[test]
fn preset_for_csv_is_code() {
    // Regression: CSV was already code, should still be.
    assert_eq!(preset_for(Some("data.csv"), None), "code");
}
