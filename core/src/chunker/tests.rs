//! Chunker unit tests.

use super::*;
use crate::chunker::formats::prose::{OVERLONG_LINE_MULTIPLIER, STRUCTURELESS_RUN_MULTIPLIER};
use crate::ids::resource_id;

/// Word-count sizer for tests — no model download required.
struct WordSizer;
impl ChunkSizer for WordSizer {
    fn size(&self, t: &str) -> usize {
        t.split_whitespace().count()
    }
}

/// Returns the char immediately preceding byte offset `pos` in `s`, if any.
fn char_before(s: &str, pos: usize) -> Option<char> {
    s[..pos].chars().next_back()
}

/// Returns the char starting at byte offset `pos` in `s`, if any.
fn char_at(s: &str, pos: usize) -> Option<char> {
    s[pos..].chars().next()
}

/// Asserts that no chunk boundary in `chunks` splits a run of alphanumeric
/// characters in `source` (a "mid-word split", #191). A boundary is a
/// mid-word split when the char immediately on one side of it and the
/// char immediately on the other side are both alphanumeric.
///
/// Deliberate scope: only alphanumeric-to-alphanumeric boundaries are flagged.
/// A split at a hyphen or apostrophe ("well-|known", "don|'t") passes silently,
/// since the flanking punctuation is not alphanumeric.
fn assert_no_mid_word_splits(source: &str, chunks: &[ChunkOutput]) {
    for c in chunks {
        let start = c.span.start;
        let end = c.span.end;
        if let (Some(prev), Some(first)) = (char_before(source, start), char_at(source, start)) {
            assert!(
                !(prev.is_alphanumeric() && first.is_alphanumeric()),
                "mid-word split at chunk start (byte {start}): preceding char {prev:?}, \
                 chunk's first char {first:?}"
            );
        }
        if let (Some(last), Some(next)) = (char_before(source, end), char_at(source, end)) {
            assert!(
                !(last.is_alphanumeric() && next.is_alphanumeric()),
                "mid-word split at chunk end (byte {end}): chunk's last char {last:?}, \
                 following char {next:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ChunkerConfig tests
// ---------------------------------------------------------------------------

#[test]
fn chunker_config_prose_defaults() {
    let cfg = ChunkerConfig::prose();
    assert_eq!(cfg.preset, "prose");
    assert_eq!(cfg.resolved_target_tokens(), 256);
    assert_eq!(cfg.resolved_overlap_tokens(), 0);
}

#[test]
fn chunker_config_code_defaults() {
    let cfg = ChunkerConfig::code();
    assert_eq!(cfg.preset, "code");
    assert_eq!(cfg.resolved_target_tokens(), 3000);
}

#[test]
fn chunker_config_from_preset_prose() {
    let cfg = ChunkerConfig::from_preset("prose").unwrap();
    assert_eq!(cfg.preset, "prose");
}

#[test]
fn chunker_config_from_preset_code() {
    let cfg = ChunkerConfig::from_preset("code").unwrap();
    assert_eq!(cfg.preset, "code");
}

#[test]
fn chunker_config_from_preset_messages_succeeds() {
    let cfg = ChunkerConfig::from_preset("messages").unwrap();
    assert_eq!(cfg.preset, "messages");
    assert_eq!(cfg.resolved_window_turns(), 6);
    assert_eq!(cfg.resolved_stride_turns(), 3);
}

#[test]
fn chunker_config_from_preset_unknown_errors() {
    let result = ChunkerConfig::from_preset("unknown_preset");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().code(), "invalid_request");
}

// ---------------------------------------------------------------------------
// Prose chunker tests
// ---------------------------------------------------------------------------

#[test]
fn prose_chunk_empty_document_returns_empty() {
    let doc_id = resource_id("file:///test.md", "abc123");
    let cfg = ChunkerConfig::prose();
    let result = chunk_prose(&doc_id, "", &cfg, &CharSizer, 0).unwrap();
    assert!(result.is_empty(), "empty doc should produce no chunks");
}

#[test]
fn prose_chunk_single_paragraph() {
    let full_text = "Hello, this is a paragraph.";
    let doc_id = resource_id("file:///test.md", "abc");
    let cfg = ChunkerConfig::prose();

    let chunks = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
    assert!(!chunks.is_empty(), "should produce at least one chunk");
    assert!(
        chunks.iter().any(|c| c.text.contains("Hello")),
        "chunk should contain the paragraph text"
    );
}

#[test]
fn prose_chunk_span_references_markdown() {
    let full_text = "# Introduction\n\nThis is the intro paragraph.";
    let doc_id = resource_id("file:///test.md", "abc");
    let cfg = ChunkerConfig::prose();

    let chunks = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
    assert!(!chunks.is_empty());
    for chunk in &chunks {
        assert!(chunk.span.start <= chunk.span.end, "span start <= end");
        assert!(!chunk.text.is_empty(), "chunk text must be non-empty");
    }
    assert!(
        chunks
            .iter()
            .any(|c| c.text.contains("Introduction") || c.text.contains("intro")),
        "chunks should contain expected text"
    );
}

#[test]
fn prose_spans_round_trip() {
    let full_text =
        "# Heading One\n\nParagraph one with some words.\n\n## Heading Two\n\nParagraph two here.";
    let doc_id = resource_id("file:///rt.md", "abc");
    let cfg = ChunkerConfig::prose();
    let chunks = chunk_prose(&doc_id, full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(!chunks.is_empty());
    for c in &chunks {
        assert!(
            c.span.start <= c.span.end,
            "span start must be <= span end (sanity check)"
        );
    }
}

#[test]
fn prose_span_slices_exactly_equal_chunk_text() {
    let full_text =
        "# Heading One\n\nParagraph one with some words.\n\n## Heading Two\n\nParagraph two here.";
    let doc_id = resource_id("file:///exact.md", "abc");
    let cfg = ChunkerConfig::prose();
    let chunks = chunk_prose(&doc_id, full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(!chunks.is_empty());
    for c in &chunks {
        assert_eq!(
            &full_text[c.span.start..c.span.end],
            c.text,
            "span slice must exactly equal chunk text"
        );
    }
}

#[test]
fn prose_adjacent_span_gaps_are_whitespace_only() {
    let para = "word ".repeat(40);
    let mut full_text = String::new();
    for i in 0..6 {
        full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
    }
    let doc_id = resource_id("file:///gaps.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(60),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "should produce multiple chunks to exercise gaps, got {}",
        chunks.len()
    );
    for pair in chunks.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert!(
            a.span.end <= b.span.start,
            "chunks must be non-overlapping and in span order: {} > {}",
            a.span.end,
            b.span.start
        );
        let gap = &full_text[a.span.end..b.span.start];
        assert!(
            gap.chars().all(|c| c.is_whitespace()),
            "gap between adjacent chunks must be whitespace-only, got: {gap:?}"
        );
    }
}

#[test]
fn prose_respects_token_target_with_word_sizer() {
    let para = "word ".repeat(40);
    let mut full_text = String::new();
    for i in 0..10 {
        full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
    }
    let doc_id = resource_id("file:///long.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(60),
        overlap_tokens: Some(8),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "long doc should produce multiple chunks, got {}",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            WordSizer.size(&c.text) <= 60,
            "chunk should respect token target: {} words",
            WordSizer.size(&c.text)
        );
    }
}

#[test]
fn prose_chunks_in_document_order() {
    let para = "word ".repeat(40);
    let mut full_text = String::new();
    for i in 0..6 {
        full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
    }
    let doc_id = resource_id("file:///order.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(60),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(chunks.len() >= 2, "should produce at least 2 chunks");
}

#[test]
fn prose_char_sizer_fallback_produces_chunks() {
    let full_text = "# Title\n\nSome prose content here for the char sizer fallback path.";
    let doc_id = resource_id("file:///char.md", "abc");
    let cfg = ChunkerConfig::prose();
    let chunks = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
    assert!(
        !chunks.is_empty(),
        "char sizer fallback should produce chunks"
    );
}

#[test]
fn prose_chunk_large_text_splits_into_multiple_chunks() {
    let para = "word ".repeat(100);
    let mut full_text = String::new();
    for i in 0..8 {
        full_text.push_str(&format!("## Para {i}\n\n{para}\n\n"));
    }
    let doc_id = resource_id("file:///large.md", "hash");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(80),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "large document should produce multiple chunks, got {}",
        chunks.len()
    );
}

#[test]
fn prose_chunk_ids_are_content_addressed() {
    let full_text = "Hello world this is content.";
    let doc_id = resource_id("file:///test.md", "abc");
    let cfg = ChunkerConfig::prose();

    let chunks1 = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();
    let chunks2 = chunk_prose(&doc_id, full_text, &cfg, &CharSizer, 0).unwrap();

    assert_eq!(chunks1.len(), chunks2.len());
    for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
        assert_eq!(c1.id, c2.id, "chunk IDs must be deterministic");
    }
}

#[test]
fn prose_chunk_heading_path_inherited_from_markdown() {
    // The splitter now sees real Markdown — heading_path is derived from the
    // Markdown heading structure, not from a Block sidecar.
    let full_text = "# API\n\nAPI documentation.\n\n# Auth\n\nAuth documentation.";
    let doc_id = resource_id("file:///api.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(8),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };

    let chunks = chunk_prose(&doc_id, full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(!chunks.is_empty());
    let with_path: Vec<_> = chunks
        .iter()
        .filter(|c| !c.heading_path.is_empty())
        .collect();
    assert!(
        !with_path.is_empty(),
        "at least one chunk should have heading_path"
    );
}

#[test]
fn prose_multibyte_utf8_no_panic() {
    let text = "こんにちは world — это тест";
    let doc_id = "doc-multibyte";
    let result = chunk_prose(doc_id, text, &ChunkerConfig::prose(), &CharSizer, 0);
    assert!(
        result.is_ok(),
        "chunking multi-byte text should not panic: {:?}",
        result.err()
    );
}

#[test]
fn prose_overlap_skipped_when_at_or_above_cap_start() {
    let para = "word ".repeat(50);
    let mut full_text = String::new();
    for i in 0..4 {
        full_text.push_str(&format!("## Section {i}\n\n{para}\n\n"));
    }
    let doc_id = resource_id("file:///overlap_guard.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(80),
        overlap_tokens: Some(60),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_prose(&doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(
        !chunks.is_empty(),
        "should produce chunks even with skipped overlap"
    );
    for w in chunks.windows(2) {
        assert!(
            w[0].span.start <= w[1].span.start,
            "chunks must be in order"
        );
    }
}

#[test]
fn prose_oversized_single_atomic_unit_no_panic() {
    let long_word = "a".repeat(2000);
    let full_text = format!("# Title\n\n{long_word}");
    let doc_id = resource_id("file:///oversized.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(20),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let result = chunk_prose(&doc_id, &full_text, &cfg, &CharSizer, 0);
    assert!(
        result.is_ok(),
        "oversized atomic unit should not panic: {:?}",
        result.err()
    );
    let chunks = result.unwrap();
    assert!(!chunks.is_empty());
}

#[test]
fn prose_splitter_sees_real_markdown_structure() {
    // Verify the splitter actually receives real Markdown (the `#` heading marker
    // must be present in chunk text so MarkdownSplitter can split on structure).
    let md = "# Section One\n\nContent of section one.\n\n# Section Two\n\nContent of section two.";
    let doc_id = resource_id("file:///structure.md", "abc");
    let cfg = ChunkerConfig {
        preset: "prose".to_string(),
        target_tokens: Some(8),
        overlap_tokens: Some(0),
        window_turns: None,
        stride_turns: None,
    };
    let chunks = chunk_prose(&doc_id, md, &cfg, &WordSizer, 0).unwrap();
    // At least one chunk should contain the `#` character (real Markdown, not stripped).
    assert!(
        chunks.iter().any(|c| c.text.contains('#')),
        "at least one chunk should contain the # heading marker; got: {:?}",
        chunks.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}

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
// Layer A: preset_for routing tests
// ---------------------------------------------------------------------------

#[test]
fn preset_for_routes_code_extensions() {
    assert_eq!(preset_for(Some("lib.rs"), None), "code");
    assert_eq!(preset_for(Some("data.json"), None), "code");
    assert_eq!(preset_for(Some("config.toml"), None), "code");
    assert_eq!(preset_for(Some("Cargo.lock"), None), "code");
    assert_eq!(preset_for(None, Some("application/json")), "code");
    assert_eq!(preset_for(None, Some("text/x-rust")), "code");
}

#[test]
fn preset_for_routes_prose() {
    assert_eq!(preset_for(Some("README.md"), None), "prose");
    assert_eq!(preset_for(Some("notes.txt"), None), "prose");
    assert_eq!(preset_for(Some("page.html"), None), "prose");
    assert_eq!(preset_for(Some("doc.pdf"), None), "prose");
    assert_eq!(preset_for(None, Some("text/plain")), "prose");
}

// ---------------------------------------------------------------------------
// Layer D: structureless and overlong line tests
// ---------------------------------------------------------------------------

#[test]
fn code_hard_splits_overlong_line() {
    // A single line of ~100k chars should produce multiple bounded chunks.
    let long_line = "x".repeat(100_000);
    let doc_id = "doc-overlong";
    let cfg = ChunkerConfig::code(); // target = 3000 chars
    let chunks = chunk_code(doc_id, &long_line, &cfg, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "overlong line should produce multiple chunks, got {}",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            c.text.chars().count() <= 3000,
            "each chunk must be within target: {} chars",
            c.text.chars().count()
        );
    }
}

#[test]
fn code_hard_split_prefers_whitespace_boundary() {
    // A single overlong line of space-separated ordinary words. The hard-split
    // path should never cut through a word (bug #191) — it should prefer to
    // split on whitespace.
    let word = "alphabet";
    let mut long_line = String::new();
    while long_line.len() < 10_000 {
        long_line.push_str(word);
        long_line.push(' ');
    }
    let doc_id = "doc-overlong-words";
    let cfg = ChunkerConfig::code(); // target = 3000 chars
    let chunks = chunk_code(doc_id, &long_line, &cfg, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "overlong line should produce multiple chunks, got {}",
        chunks.len()
    );
    assert_no_mid_word_splits(&long_line, &chunks);
}

#[test]
fn code_hard_split_no_whitespace_falls_back_to_char_cut() {
    // An overlong line with NO whitespace at all (e.g. base64) must still be
    // hard-split at the char target — there's no whitespace to back off to, so the
    // "no whitespace found in window" branch of the (b) fix must fall through to the
    // original hard char cut, unchanged. Both branches of the whitespace-backoff
    // logic must be covered: this test pins the fallback branch, while
    // `code_hard_split_prefers_whitespace_boundary` pins the whitespace-preferring one.
    let alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let long_line: String = alphabet.chars().cycle().take(10_000).collect();
    assert!(
        !long_line.chars().any(|c| c.is_whitespace()),
        "fixture must contain no whitespace"
    );
    let doc_id = "doc-no-whitespace";
    let cfg = ChunkerConfig::code(); // target = 3000 chars
    let chunks = chunk_code(doc_id, &long_line, &cfg, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "overlong whitespace-free line should still produce multiple chunks, got {}",
        chunks.len()
    );
    for c in &chunks {
        assert!(
            c.text.chars().count() <= 3000,
            "each chunk must be within target: {} chars",
            c.text.chars().count()
        );
    }
    // Hard char cuts must be lossless and contiguous — reassembling every chunk's
    // text must exactly reproduce the original line.
    let reassembled: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(reassembled, long_line, "hard char cuts must be lossless");
}

#[test]
fn prose_long_single_line_paragraph_does_not_split_mid_word() {
    // A single-line paragraph (no newlines) of ordinary English sentences,
    // long enough to trip the Layer D backstop (> 8 * target chars) and be
    // delegated to chunk_code, whose hard-split path must not cut mid-word
    // (bug #191).
    let sentence =
        "The quick brown fox jumps over the lazy dog and runs swiftly through the forest. ";
    let mut full_text = String::new();
    while full_text.len() < 2200 {
        full_text.push_str(sentence);
    }
    assert!(!full_text.contains('\n'), "paragraph must be a single line");
    let doc_id = "doc-prose-long-line";
    let cfg = ChunkerConfig::prose(); // target = 256 chars; backstop threshold = 2048
    let chunks = chunk_prose(doc_id, &full_text, &cfg, &WordSizer, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "long single-line paragraph should produce multiple chunks, got {}",
        chunks.len()
    );
    assert_no_mid_word_splits(&full_text, &chunks);
}

#[test]
fn prose_overlong_single_line_hits_perf_guard_no_hang_no_mid_word_splits() {
    // Layer D performance guard (`OVERLONG_LINE_MULTIPLIER`): a pathologically long
    // single LINE — even one full of ordinary whitespace-separated words — must not
    // reach MarkdownSplitter, whose split-point search is super-linear on one flat
    // line (measured ~O(n²): 4.2s at 800k chars; the #61 hang class). At 200k chars
    // this line is far above the 64×target (16 384-char) guard, so it routes to
    // `chunk_code` — which, post-#191, backs its hard splits off to whitespace, so
    // even this degraded path must produce no mid-word splits. Completing promptly
    // (chunk_code is O(n)) is itself a key assertion.
    let long_line = "word ".repeat(40_000); // ~200k chars, no newlines
    let doc_id = "doc-overlong-line";
    let cfg = ChunkerConfig::prose(); // target = 256; line guard = 16_384 chars
    let target = cfg.resolved_target_tokens();
    let chunks = chunk_prose(doc_id, &long_line, &cfg, &CharSizer, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "overlong single line should split into multiple chunks, got {}",
        chunks.len()
    );
    // chunk_code bounds every chunk to ≤ target chars — the observable pinning that
    // the perf guard routed this block to chunk_code, not MarkdownSplitter.
    for c in &chunks {
        assert!(
            c.text.chars().count() <= target,
            "chunk_code path should bound every chunk to the char target: {} chars",
            c.text.chars().count()
        );
    }
    assert_no_mid_word_splits(&long_line, &chunks);
}

#[test]
fn prose_long_single_line_below_perf_guard_stays_on_prose_path() {
    // Boundary of the Layer D dual probe: a single-line paragraph well above the old
    // 8×target line threshold but below the new 64×target perf guard must stay on
    // the semantic MarkdownSplitter path — this is the #191 quality win. Observable:
    // with WordSizer (256-word cap) the prose path emits chunks far longer than 256
    // CHARS, whereas the chunk_code path bounds every chunk to ≤ 256 chars.
    let sentence = "The quick brown fox jumps over the lazy dog near the riverbank today. ";
    let long_line = sentence.repeat(75).trim_end().to_string(); // ~5.3k chars, one line
    assert!(!long_line.contains('\n'));
    let cfg = ChunkerConfig::prose();
    let target = cfg.resolved_target_tokens();
    // The line is far longer than 8×target chars, i.e. it would have tripped the
    // pre-#191 line-length probe; sanity-check that NEITHER current probe trips
    // (these mirror the actual backstop branch conditions in `chunk_prose`):
    assert!(long_line.chars().count() > STRUCTURELESS_RUN_MULTIPLIER * target);
    let max_run = long_line
        .split_whitespace()
        .map(|w| w.chars().count())
        .max()
        .unwrap();
    assert!(max_run <= STRUCTURELESS_RUN_MULTIPLIER * target);
    assert!(long_line.chars().count() <= OVERLONG_LINE_MULTIPLIER * target);
    let chunks = chunk_prose("doc-below-guard", &long_line, &cfg, &WordSizer, 0).unwrap();
    assert!(chunks.len() >= 2, "expected multiple chunks");
    assert!(
        chunks.iter().any(|c| c.text.chars().count() > target),
        "prose path should pack chunks beyond the char target (word-capped, not char-capped)"
    );
    assert_no_mid_word_splits(&long_line, &chunks);
}

#[test]
fn prose_cjk_long_paragraph_pins_char_cut_limitation() {
    // Pins a DOCUMENTED limitation, not desired behavior: scripts without inter-word
    // whitespace (CJK, Thai, …) make an entire paragraph one whitespace-free "run",
    // so long CJK prose trips the structureless probe and is routed to `chunk_code`,
    // where the whitespace backoff never fires and the raw char cut applies — i.e.
    // CJK text still gets mid-"word" cuts (#191 fixes whitespace-delimited scripts
    // only; word segmentation is out of scope). What this pins: the routing, that
    // chunking completes promptly, char-boundary safety on multibyte text, the
    // exact-slice span invariant, and lossless reassembly. Deliberately does NOT use
    // `assert_no_mid_word_splits` — mid-word cuts are expected here.
    let sentence = "深度学习模型通过大量标注数据进行训练以逐步提高预测准确性。";
    let cfg = ChunkerConfig::prose();
    let target = cfg.resolved_target_tokens();
    let para = sentence.repeat(2048 / sentence.chars().count() + 2); // > 8×target chars
    assert!(para.chars().count() > STRUCTURELESS_RUN_MULTIPLIER * target);
    assert!(!para.contains(char::is_whitespace));
    let chunks = chunk_prose("doc-cjk", &para, &cfg, &CharSizer, 0).unwrap();
    assert!(chunks.len() >= 2, "expected multiple chunks");
    let mut reassembled = String::new();
    for c in &chunks {
        // chunk_code path: every chunk bounded to ≤ target chars.
        assert!(c.text.chars().count() <= target);
        // Exact-slice span invariant holds even on the char-cut path.
        assert_eq!(&para[c.span.start..c.span.end], c.text);
        reassembled.push_str(&c.text);
    }
    // No whitespace to trim and no gaps possible: reassembly is lossless.
    assert_eq!(reassembled, para);
}

#[test]
fn prose_embedded_long_token_still_uses_line_packer() {
    // A paragraph that is otherwise ordinary prose but contains ONE embedded token
    // far longer than 8x the char target, with no internal whitespace (e.g. a URL or
    // base64 blob). This is genuine structurelessness (part (a)'s accepted
    // limitation) — the backstop must still catch it and delegate the WHOLE block to
    // chunk_code, whose hard-split path is the only way to bound the token's own
    // size (mid-token cuts are unavoidable for a space-free run this long).
    let target = ChunkerConfig::prose().resolved_target_tokens(); // 256
    let huge_token = "a".repeat(target * 9); // safely over the 8x backstop threshold
    let full_text =
        format!("Some ordinary prose leads into a huge token: {huge_token} and then it ends.");
    let doc_id = "doc-embedded-token";
    let cfg = ChunkerConfig::prose();
    let chunks = chunk_prose(doc_id, &full_text, &cfg, &CharSizer, 0).unwrap();
    assert!(
        chunks.len() >= 2,
        "backstop should route to chunk_code and hard-split the oversized token, got {} chunks",
        chunks.len()
    );
    // chunk_code bounds every chunk to at most `target` chars (its hard-cut budget) —
    // that bound is the observable pinning "routed to chunk_code" vs. "handled
    // directly by MarkdownSplitter" (which sizes chunks by the sizer's own metric,
    // not a hard char budget).
    for c in &chunks {
        assert!(
            c.text.chars().count() <= target,
            "chunk_code path should bound every chunk to the char target: {} chars",
            c.text.chars().count()
        );
    }
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
