use super::*;

// `clamp_search_limit`/`SEARCH_MAX_LIMIT` are re-exported from
// `localdb_core::search` (issue #187 review) — their own coverage lives
// in `localdb_core::search::tests::clamp_search_limit_*` rather than
// being duplicated here.

#[test]
fn resolve_page_end_adds_offset_and_limit() {
    assert_eq!(resolve_page_end(3, 7).unwrap(), 10);
    assert_eq!(resolve_page_end(0, 0).unwrap(), 0);
}

#[test]
fn resolve_page_end_rejects_overflow_as_invalid_request() {
    let err = resolve_page_end(usize::MAX, 1).expect_err("overflow must be rejected");
    match err.0 {
        CoreError::InvalidRequest { .. } => {}
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}
