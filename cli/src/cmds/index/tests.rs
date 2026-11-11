use super::*;

fn outcome(name: &str, summary: IndexSummary) -> StoreIndexOutcome {
    StoreIndexOutcome {
        store_name: name.to_string(),
        summary,
        job_id: None,
    }
}

/// Like [`outcome`], but with an explicit `job_id` — for the
/// `job_id`-in-JSON tests below.
fn outcome_with_job(name: &str, summary: IndexSummary, job_id: &str) -> StoreIndexOutcome {
    StoreIndexOutcome {
        store_name: name.to_string(),
        summary,
        job_id: Some(job_id.to_string()),
    }
}

fn with_sources(
    indexed: u64,
    skipped: u64,
    chunks: u64,
    unsupported: u64,
    errors: u64,
) -> IndexSummary {
    IndexSummary {
        has_sources: true,
        indexed,
        skipped,
        chunks,
        errors,
        unsupported,
        prunable: 0,
        deleted: 0,
        metadata_updated: 0,
        feed_liveness_checked: 0,
        recheck_deferred: 0,
    }
}

// -- total_summary --------------------------------------------------

#[test]
fn total_summary_sums_fields_across_stores() {
    let outcomes = vec![
        outcome("a", with_sources(3, 1, 6, 0, 0)),
        outcome("b", with_sources(1, 0, 2, 1, 2)),
    ];
    let total = total_summary(&outcomes);
    assert_eq!(total.indexed, 4);
    assert_eq!(total.skipped, 1);
    assert_eq!(total.chunks, 8);
    assert_eq!(total.unsupported, 1);
    assert_eq!(total.errors, 2);
    assert!(total.has_sources);
}

#[test]
fn total_summary_has_sources_false_when_no_store_has_sources() {
    let outcomes = vec![
        outcome("a", IndexSummary::default()),
        outcome("b", IndexSummary::default()),
    ];
    assert!(!total_summary(&outcomes).has_sources);
}

#[test]
fn total_summary_has_sources_true_when_any_store_has_sources() {
    let outcomes = vec![
        outcome("a", IndexSummary::default()),
        outcome("b", with_sources(1, 0, 1, 0, 0)),
    ];
    assert!(total_summary(&outcomes).has_sources);
}

#[test]
fn total_summary_empty_outcomes_is_default() {
    assert_eq!(total_summary(&[]), IndexSummary::default());
}

// -- strict_should_fail -----------------------------------------------

#[test]
fn strict_should_fail_false_without_strict_flag() {
    let outcomes = vec![outcome("a", with_sources(0, 0, 0, 0, 5))];
    assert!(!strict_should_fail(&outcomes, false));
}

#[test]
fn strict_should_fail_false_with_strict_flag_and_no_errors() {
    let outcomes = vec![outcome("a", with_sources(3, 0, 6, 0, 0))];
    assert!(!strict_should_fail(&outcomes, true));
}

#[test]
fn strict_should_fail_true_when_any_store_errored() {
    let outcomes = vec![
        outcome("a", with_sources(3, 0, 6, 0, 0)),
        outcome("b", with_sources(1, 0, 2, 0, 1)),
    ];
    assert!(strict_should_fail(&outcomes, true));
}

// -- render_index_text --------------------------------------------------

#[test]
fn render_index_text_single_store_matches_legacy_format() {
    let outcomes = vec![outcome("books", with_sources(3, 1, 6, 0, 0))];
    assert_eq!(
        render_index_text(&outcomes),
        "Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors"
    );
}

#[test]
fn render_index_text_single_store_no_sources_matches_legacy_format() {
    let outcomes = vec![outcome("books", IndexSummary::default())];
    assert_eq!(
        render_index_text(&outcomes),
        "No sources to index on store 'books'."
    );
}

#[test]
fn render_index_text_multi_store_prefixes_and_appends_total() {
    let outcomes = vec![
        outcome("books", with_sources(3, 1, 6, 0, 0)),
        outcome("notes", IndexSummary::default()),
    ];
    let rendered = render_index_text(&outcomes);
    assert_eq!(
        rendered,
        "[books] Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors\n\
             [notes] No sources to index.\n\
             Total: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors"
    );
}

/// `metadata_updated` follows the same conditional-append precedent as
/// `deleted`/`prunable` above: it only appears in the sentence when
/// non-zero, so the base (zero) case stays byte-identical to the pinned
/// legacy format — see the three tests above, none of which mention
/// "metadata updated" and none of which needed to change for this field
/// to exist.
#[test]
fn render_index_text_appends_metadata_updated_when_nonzero() {
    let mut summary = with_sources(3, 1, 6, 0, 0);
    summary.metadata_updated = 2;
    let outcomes = vec![outcome("books", summary)];
    assert_eq!(
        render_index_text(&outcomes),
        "Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors, \
             2 metadata updated"
    );
}

// -- render_index_json --------------------------------------------------

#[test]
fn render_index_json_includes_metadata_updated_field() {
    let mut summary = with_sources(3, 1, 6, 0, 0);
    summary.metadata_updated = 2;
    let outcomes = vec![outcome("books", summary)];
    let v = render_index_json(&outcomes, false);
    assert_eq!(v["docs_metadata_updated"], json!(2));
}

#[test]
fn total_summary_sums_metadata_updated_across_stores() {
    let mut a = with_sources(3, 1, 6, 0, 0);
    a.metadata_updated = 1;
    let mut b = with_sources(1, 0, 2, 1, 2);
    b.metadata_updated = 4;
    let outcomes = vec![outcome("a", a), outcome("b", b)];
    assert_eq!(total_summary(&outcomes).metadata_updated, 5);
}

#[test]
fn render_index_text_appends_feed_liveness_checked_when_nonzero() {
    let mut summary = with_sources(3, 1, 6, 0, 0);
    summary.feed_liveness_checked = 5;
    let outcomes = vec![outcome("books", summary)];
    assert_eq!(
        render_index_text(&outcomes),
        "Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors, \
             5 feed entries checked for liveness"
    );
}

#[test]
fn render_index_json_includes_feed_entries_liveness_checked_field() {
    let mut summary = with_sources(3, 1, 6, 0, 0);
    summary.feed_liveness_checked = 5;
    let outcomes = vec![outcome("books", summary)];
    let v = render_index_json(&outcomes, false);
    assert_eq!(v["feed_entries_liveness_checked"], json!(5));
}

#[test]
fn total_summary_sums_feed_liveness_checked_across_stores() {
    let mut a = with_sources(3, 1, 6, 0, 0);
    a.feed_liveness_checked = 1;
    let mut b = with_sources(1, 0, 2, 1, 2);
    b.feed_liveness_checked = 4;
    let outcomes = vec![outcome("a", a), outcome("b", b)];
    assert_eq!(total_summary(&outcomes).feed_liveness_checked, 5);
}

/// Same append-only-when-nonzero convention as `metadata_updated`/
/// `feed_liveness_checked` above: the base (zero) case stays
/// byte-identical to the pinned legacy format.
#[test]
fn render_index_text_omits_recheck_deferred_when_zero() {
    let outcomes = vec![outcome("books", with_sources(3, 1, 6, 0, 0))];
    assert_eq!(
        render_index_text(&outcomes),
        "Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors"
    );
}

#[test]
fn render_index_text_appends_recheck_deferred_when_nonzero() {
    let mut summary = with_sources(3, 1, 6, 0, 0);
    summary.recheck_deferred = 5;
    let outcomes = vec![outcome("books", summary)];
    assert_eq!(
        render_index_text(&outcomes),
        "Index complete: 3 indexed, 1 skipped, 6 chunks written, 0 unsupported, 0 errors, \
             5 rechecks deferred"
    );
}

/// Unlike the append-only-when-nonzero text rendering, `--json` carries
/// `docs_recheck_deferred` unconditionally (default 0) — specs/05-
/// surfaces.md "`localdb index` — recheck gate".
#[test]
fn render_index_json_always_includes_docs_recheck_deferred_field() {
    let outcomes = vec![outcome("books", with_sources(3, 1, 6, 0, 0))];
    let v = render_index_json(&outcomes, false);
    assert_eq!(v["docs_recheck_deferred"], json!(0));
}

#[test]
fn render_index_json_includes_nonzero_docs_recheck_deferred_field() {
    let mut summary = with_sources(3, 1, 6, 0, 0);
    summary.recheck_deferred = 5;
    let outcomes = vec![outcome("books", summary)];
    let v = render_index_json(&outcomes, false);
    assert_eq!(v["docs_recheck_deferred"], json!(5));
}

#[test]
fn total_summary_sums_recheck_deferred_across_stores() {
    let mut a = with_sources(3, 1, 6, 0, 0);
    a.recheck_deferred = 1;
    let mut b = with_sources(1, 0, 2, 1, 2);
    b.recheck_deferred = 4;
    let outcomes = vec![outcome("a", a), outcome("b", b)];
    assert_eq!(total_summary(&outcomes).recheck_deferred, 5);
}

#[test]
fn render_index_json_single_store_matches_legacy_flat_shape() {
    let outcomes = vec![outcome("books", with_sources(3, 1, 6, 0, 0))];
    let v = render_index_json(&outcomes, false);
    assert_eq!(
        v,
        json!({
            "status": "ok",
            "docs_indexed": 3,
            "docs_skipped": 1,
            "chunks_written": 6,
            "unsupported": 0,
            "errors": 0,
            // Added alongside the opt-in `--delete` flag: a retaining run
            // has to be able to tell consumers what pruning would remove.
            "docs_deleted": 0,
            "docs_prunable": 0,
            "docs_metadata_updated": 0,
            "feed_entries_liveness_checked": 0,
            "docs_recheck_deferred": 0,
        })
    );
    assert!(
        v.get("store").is_none(),
        "single-store JSON must not gain a store field"
    );
}

/// A single-store outcome carrying a job id
/// gains a `job_id` field in the flat JSON shape.
#[test]
fn render_index_json_single_store_includes_job_id_when_present() {
    let outcomes = vec![outcome_with_job(
        "books",
        with_sources(3, 1, 6, 0, 0),
        "01HRQHB7FN3WMX4AZDV3S9VCTZ",
    )];
    let v = render_index_json(&outcomes, false);
    assert_eq!(v["job_id"], json!("01HRQHB7FN3WMX4AZDV3S9VCTZ"));
}

/// A store with no sources never submitted a job at all — no `job_id`
/// key at all, not `null`, preserving the exact pre-existing shape.
#[test]
fn render_index_json_no_sources_never_gains_a_job_id_key() {
    let outcomes = vec![outcome("books", IndexSummary::default())];
    let v = render_index_json(&outcomes, false);
    assert!(
        v.get("job_id").is_none(),
        "a store with no sources never submitted a job, so no job_id key should appear"
    );
}

#[test]
fn render_index_json_single_store_no_sources_matches_legacy_shape() {
    let outcomes = vec![outcome("books", IndexSummary::default())];
    let v = render_index_json(&outcomes, false);
    assert_eq!(
        v,
        json!({ "status": "ok", "message": "no sources to index" })
    );
}

#[test]
fn render_index_json_multi_store_wraps_with_total() {
    let outcomes = vec![
        outcome("books", with_sources(3, 1, 6, 0, 0)),
        outcome("notes", with_sources(1, 0, 2, 0, 1)),
    ];
    let v = render_index_json(&outcomes, false);
    assert_eq!(
        v,
        json!({
            "stores": [
                {
                    "store": "books",
                    "status": "ok",
                    "docs_indexed": 3,
                    "docs_skipped": 1,
                    "chunks_written": 6,
                    "unsupported": 0,
                    "errors": 0,
                    "docs_deleted": 0,
                    "docs_prunable": 0,
                    "docs_metadata_updated": 0,
                    "feed_entries_liveness_checked": 0,
                    "docs_recheck_deferred": 0,
                },
                {
                    "store": "notes",
                    "status": "ok",
                    "docs_indexed": 1,
                    "docs_skipped": 0,
                    "chunks_written": 2,
                    "unsupported": 0,
                    "errors": 1,
                    "docs_deleted": 0,
                    "docs_prunable": 0,
                    "docs_metadata_updated": 0,
                    "feed_entries_liveness_checked": 0,
                    "docs_recheck_deferred": 0,
                },
            ],
            "total": {
                "status": "ok",
                "docs_indexed": 4,
                "docs_skipped": 1,
                "chunks_written": 8,
                "unsupported": 0,
                "errors": 1,
                "docs_deleted": 0,
                "docs_prunable": 0,
                "docs_metadata_updated": 0,
                "feed_entries_liveness_checked": 0,
                "docs_recheck_deferred": 0,
            },
        })
    );
}

/// Each multi-store entry carries its own
/// `job_id` (each store submitted a genuinely distinct job); `total`
/// never gets one, since it spans every contributing job.
#[test]
fn render_index_json_multi_store_carries_a_job_id_per_store_but_never_on_total() {
    let outcomes = vec![
        outcome_with_job("books", with_sources(3, 1, 6, 0, 0), "job-books"),
        outcome_with_job("notes", with_sources(1, 0, 2, 0, 1), "job-notes"),
    ];
    let v = render_index_json(&outcomes, false);
    assert_eq!(v["stores"][0]["job_id"], json!("job-books"));
    assert_eq!(v["stores"][1]["job_id"], json!("job-notes"));
    assert!(
        v["total"].get("job_id").is_none(),
        "the combined total must never carry a single job_id"
    );
}

#[test]
fn render_index_json_strict_marks_errored_stores_and_total() {
    let outcomes = vec![
        outcome("books", with_sources(3, 0, 6, 0, 0)),
        outcome("notes", with_sources(1, 0, 2, 0, 1)),
    ];
    let v = render_index_json(&outcomes, true);
    assert_eq!(v["stores"][0]["status"], "ok");
    assert_eq!(v["stores"][1]["status"], "error");
    assert_eq!(v["total"]["status"], "error");
}

#[test]
fn render_index_json_multi_store_all_without_sources() {
    let outcomes = vec![
        outcome("books", IndexSummary::default()),
        outcome("notes", IndexSummary::default()),
    ];
    let v = render_index_json(&outcomes, false);
    assert_eq!(
        v["stores"][0],
        json!({ "store": "books", "status": "ok", "message": "no sources to index" })
    );
    assert_eq!(
        v["total"],
        json!({ "status": "ok", "message": "no sources to index" })
    );
}
