//! Prose chunker tests, including the Layer D structureless/overlong-line backstop.

use super::common::{assert_no_mid_word_splits, WordSizer};
use crate::chunker::formats::code::chunk_code;
use crate::chunker::formats::prose::{
    chunk_prose, OVERLONG_LINE_MULTIPLIER, STRUCTURELESS_RUN_MULTIPLIER,
};
use crate::chunker::{CharSizer, ChunkSizer, ChunkerConfig};
use crate::ids::resource_id;

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
