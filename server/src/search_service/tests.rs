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

#[test]
fn search_dedup_request_default_modes_and_invalid_values() {
    let default: SearchRequest = serde_json::from_value(serde_json::json!({"query":"x"})).unwrap();
    assert_eq!(default.dedup, localdb_core::SearchDedup::TextAndVector);
    for mode in ["off", "text", "text_and_vector"] {
        let request: SearchRequest =
            serde_json::from_value(serde_json::json!({"query":"x", "dedup":mode})).unwrap();
        assert_eq!(request.dedup.to_string(), mode);
    }
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!(false),
        serde_json::json!("approximate"),
    ] {
        assert!(serde_json::from_value::<SearchRequest>(
            serde_json::json!({"query":"x", "dedup":invalid})
        )
        .is_err());
    }
}
