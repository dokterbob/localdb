//! Difficult-PDF corpus test.
//!
//! Runs `extract::pdf::extract_pdf` against a corpus of deliberately-hard,
//! freely-licensed PDFs and checks each against an expectation file whose
//! ground truth was transcribed by an agent reading the *rendered page images*
//! (independent of any text extractor). See `tests/fixtures/corpus/README.md`.
//!
//! The redistributable PDFs are committed; `sewtha-sustainable-energy.pdf` is
//! fetch-on-demand (`scripts/fetch_test_pdfs.sh`) and its case is skipped when
//! absent — so this test always runs, doing more when the full corpus is
//! present.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// A phrase is "found" if it appears in the extracted text after collapsing all
/// runs of whitespace to single spaces on both sides — tolerates the line-wrap
/// and inter-span spacing differences between visual layout and reading order,
/// without masking dropped or garbled words.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Small, clean `"ok"` documents must extract at least this fraction of the
/// agent-transcribed phrases. Large books and reading-order-complex layouts are
/// exempt (see `phrase_recall_applies`).
const PHRASE_RECALL_FLOOR: f64 = 0.8;

/// Phrase recall is only a meaningful gate for small, clean, text-ordered
/// documents. For a long book or a dense multi-column journal the extractor's
/// reading order legitimately differs from the visual order a human (or the
/// image-reading agent) follows, so exact-phrase recall understates quality.
/// Those still get the no-panic, page-count, and anti-mojibake checks.
fn phrase_recall_applies(expect: &str, pages: u64, total_phrases: usize) -> bool {
    expect == "ok" && pages <= 12 && total_phrases >= 3
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/corpus")
}

fn all_phrases(exp: &Value) -> Vec<String> {
    exp.get("page_phrases")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.values()
                .filter_map(|arr| arr.as_array())
                .flatten()
                .filter_map(|p| p.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Drive one expectation file. Returns `false` if skipped (PDF absent).
fn check_one(exp_path: &Path) -> bool {
    let exp: Value = serde_json::from_str(
        &std::fs::read_to_string(exp_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", exp_path.display())),
    )
    .unwrap_or_else(|e| panic!("parse {}: {e}", exp_path.display()));

    let file = exp["file"].as_str().expect("expectation.file");
    let expect = exp["expect"].as_str().unwrap_or("any");
    let pages = exp["pages"].as_u64().unwrap_or(0);
    let label = file;

    let pdf_path = corpus_dir().join(file);
    if !pdf_path.exists() {
        // Ignored-if-absent (fetch-on-demand files like sewtha.pdf).
        eprintln!("corpus: skipping {label} (PDF absent — run scripts/fetch_test_pdfs.sh)");
        return false;
    }

    let bytes = std::fs::read(&pdf_path).unwrap_or_else(|e| panic!("read {label}: {e}"));
    // A panic here is a real failure — malformed PDFs must never panic (#87).
    let result = extract::pdf::extract_pdf(&bytes);

    match expect {
        "ok" => assert!(
            result.is_ok(),
            "{label}: expected successful extraction, got {result:?}"
        ),
        "err" => assert!(
            result.is_err(),
            "{label}: expected an extraction error, got Ok"
        ),
        "any" => {}
        other => panic!("{label}: unknown expect value {other:?}"),
    }

    let Ok(extracted) = result else {
        // An accepted error (expect "err"/"any"): nothing more to check.
        return true;
    };

    let forbid: Vec<&str> = exp
        .get("forbid_substrings")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
        .unwrap_or_default();
    for bad in &forbid {
        if bad.is_empty() {
            continue;
        }
        assert!(
            !extracted.markdown.contains(bad),
            "{label}: forbidden substring {bad:?} appeared in extracted text (mojibake?)"
        );
    }
    // The anti-mojibake guarantee is unconditional, even when a doc's
    // expectation omitted the replacement char from forbid_substrings.
    assert!(
        !extracted.markdown.contains('\u{FFFD}'),
        "{label}: extracted text contains U+FFFD replacement char"
    );

    // Structural page check for paginated "ok" documents.
    if expect == "ok" && pages > 0 {
        let n = extracted.page_starts.len();
        assert!(n >= 1, "{label}: ok PDF produced no page offsets");
        assert!(
            n as u64 <= pages,
            "{label}: {n} page offsets exceeds the {pages}-page document"
        );
        // Offsets strictly ascending, page numbers strictly ascending, in bounds.
        for w in extracted.page_starts.windows(2) {
            assert!(w[0].0 < w[1].0, "{label}: page offsets must ascend");
            assert!(w[0].1 < w[1].1, "{label}: page numbers must ascend");
        }
        if let Some(&(off, _)) = extracted.page_starts.last() {
            assert!(
                off < extracted.markdown.len(),
                "{label}: last page offset out of bounds"
            );
        }
    }

    // Phrase recall for small, clean documents.
    let phrases = all_phrases(&exp);
    if phrase_recall_applies(expect, pages, phrases.len()) {
        let haystack = normalize_ws(&extracted.markdown);
        let found = phrases
            .iter()
            .filter(|p| haystack.contains(&normalize_ws(p)))
            .count();
        let recall = found as f64 / phrases.len() as f64;
        assert!(
            recall >= PHRASE_RECALL_FLOOR,
            "{label}: phrase recall {recall:.2} below floor {PHRASE_RECALL_FLOOR:.2} \
             ({found}/{} transcribed phrases found)",
            phrases.len()
        );
    }

    true
}

#[test]
fn corpus_extraction_meets_ground_truth() {
    let dir = corpus_dir();
    let mut checked = 0usize;
    let mut skipped = 0usize;

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read corpus dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && p.file_name().is_some_and(|n| n != "manifest.json")
        })
        .collect();
    entries.sort();

    assert!(
        !entries.is_empty(),
        "no corpus expectation files found in {}",
        dir.display()
    );

    for exp in entries {
        if check_one(&exp) {
            checked += 1;
        } else {
            skipped += 1;
        }
    }

    // The redistributable corpus is committed, so at least those must run even
    // when the fetch-on-demand files are absent.
    assert!(
        checked >= 9,
        "expected ≥9 committed corpus PDFs to be checked, only {checked} ran ({skipped} skipped)"
    );
    eprintln!("corpus: {checked} checked, {skipped} skipped (fetch-on-demand absent)");
}
