//! `get_chunks` tool tests: pagination, sorting, and the search→get_chunks
//! chaining path. Anchor-relative pagination (#146) lives in
//! `anchor_pagination.rs`.

use serde_json::{json, Value};

use rmcp::service::{RoleClient, RunningService};

use crate::harness::{
    call_tool, client_for, make_handler_with_multichunk_doc, make_handler_with_one_store,
    make_handler_with_seeded_store, make_handler_with_tied_chunks, text_of,
};

/// get_chunks returns chunks sorted by (block_seq, seq_in_block) regardless
/// of insertion order, with correct spans and heading_path.
#[tokio::test]
async fn test_get_chunks_happy_path_sorted() {
    let (handler, doc_id) = make_handler_with_multichunk_doc().await;
    let client = client_for(handler).await;

    let result = call_tool(&client, "get_chunks", json!({ "resource_id": doc_id }))
        .await
        .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).expect("valid JSON in content");

    assert_eq!(parsed["resource_id"], doc_id);
    assert_eq!(parsed["uri"], "file:///docs/multi.md");
    assert_eq!(parsed["title"], "Multi-chunk Doc");
    assert_eq!(parsed["total_chunks"], 3);
    assert_eq!(parsed["offset"], 0);
    assert_eq!(parsed["returned"], 3);

    let chunks = parsed["chunks"].as_array().expect("chunks array");
    assert_eq!(chunks.len(), 3);

    assert_eq!(chunks[0]["text"], "first chunk text");
    assert_eq!(chunks[0]["block_seq"], 0);
    assert_eq!(chunks[0]["seq_in_block"], 0);
    assert_eq!(chunks[0]["heading_path"][0], "Section One");
    assert_eq!(chunks[0]["span"]["start"], 0);
    assert_eq!(chunks[0]["span"]["end"], "first chunk text".len());
    assert_eq!(chunks[0]["block_kind"], "text");

    assert_eq!(chunks[1]["text"], "second chunk text");
    assert_eq!(chunks[1]["block_seq"], 1);
    assert_eq!(chunks[1]["seq_in_block"], 0);

    assert_eq!(chunks[2]["text"], "third chunk text");
    assert_eq!(chunks[2]["block_seq"], 1);
    assert_eq!(chunks[2]["seq_in_block"], 1);
}

/// get_chunks paginates with offset/limit.
#[tokio::test]
async fn test_get_chunks_pagination_offset_limit() {
    let (handler, doc_id) = make_handler_with_multichunk_doc().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "offset": 1, "limit": 1 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["total_chunks"], 3);
    assert_eq!(parsed["offset"], 1);
    assert_eq!(parsed["limit"], 1);
    assert_eq!(parsed["returned"], 1);

    let chunks = parsed["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["text"], "second chunk text");
}

/// get_chunks with an out-of-range offset returns an empty chunks array,
/// not an error.
#[tokio::test]
async fn test_get_chunks_offset_out_of_range_returns_empty() {
    let (handler, doc_id) = make_handler_with_multichunk_doc().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "offset": 99 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(
        result.is_error,
        Some(false),
        "out-of-range offset is not an error"
    );

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["total_chunks"], 3);
    assert_eq!(parsed["returned"], 0);
    assert!(parsed["chunks"].as_array().unwrap().is_empty());
}

/// get_chunks with missing resource_id (changed expectation): now fails
/// `Parameters<GetChunksArgs>` deserialization (`resource_id` is required)
/// — a tool-level "failed to deserialize parameters" error.
#[tokio::test]
async fn test_get_chunks_missing_resource_id() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = call_tool(&client, "get_chunks", json!({})).await;
    let text = crate::harness::assert_deserialization_error(result);
    assert!(
        text.contains("resource_id"),
        "error should mention 'resource_id': {text}"
    );
}

/// get_chunks with an unknown resource_id → resource_not_found tool error.
#[tokio::test]
async fn test_get_chunks_unknown_resource_id() {
    let (handler, _doc_id) = make_handler_with_multichunk_doc().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": "nonexistent-doc" }),
    )
    .await
    .expect("call succeeds at the protocol level");
    assert_eq!(result.is_error, Some(true), "should be a tool error");

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        parsed["error"]["code"].as_str().unwrap(),
        "resource_not_found"
    );
}

/// Chaining test: `search` → take `citations[0].resource_id` → `get_chunks`.
/// Proves that `Citation.resource_id` is sufficient to drive `get_chunks`.
#[tokio::test]
async fn test_search_to_get_chunks_chaining() {
    let (handler, expected_doc_id, _chunk_id) = make_handler_with_seeded_store().await;
    let client = client_for(handler).await;

    let search_result = call_tool(
        &client,
        "search",
        json!({ "query": "Rust programming language", "limit": 5 }),
    )
    .await
    .expect("search succeeds");

    let text = text_of(&search_result);
    let json_part = text.split("\n---\n").next().unwrap_or(&text);
    let parsed: Value = serde_json::from_str(json_part).unwrap();
    let citations = parsed["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "search should find the seeded chunk");

    let resource_id = citations[0]["resource_id"]
        .as_str()
        .expect("citation.resource_id must be a string")
        .to_string();
    assert_eq!(resource_id, expected_doc_id);

    let chunks_result = call_tool(&client, "get_chunks", json!({ "resource_id": resource_id }))
        .await
        .expect("get_chunks succeeds");
    assert_eq!(
        chunks_result.is_error,
        Some(false),
        "get_chunks should succeed"
    );

    let text = text_of(&chunks_result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["resource_id"], expected_doc_id);
    assert_eq!(parsed["total_chunks"], 1);
}

/// get_chunks imposes a stable total order even when chunks tie on
/// `(block_seq, seq_in_block)`. Two `(0, 0)` chunks with an identical span
/// but different ids must paginate identically across repeated calls AND
/// regardless of the order the backend returns them in (proven by seeding
/// the same pair in opposite insertion orders). The tie is broken by
/// `chunk_id`.
#[tokio::test]
async fn test_get_chunks_deterministic_tie_breaker() {
    async fn ordered_ids(client: &RunningService<RoleClient, ()>, doc_id: &str) -> Vec<String> {
        let result = call_tool(client, "get_chunks", json!({ "resource_id": doc_id }))
            .await
            .expect("get_chunks succeeds");
        assert_eq!(result.is_error, Some(false));
        let text = text_of(&result);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        parsed["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["chunk_id"].as_str().unwrap().to_string())
            .collect()
    }

    let (handler_fwd, doc_id) = make_handler_with_tied_chunks(false).await;
    let client_fwd = client_for(handler_fwd).await;
    let (handler_rev, _doc_id_rev) = make_handler_with_tied_chunks(true).await;
    let client_rev = client_for(handler_rev).await;

    // Repeated calls on the same server are stable.
    let first = ordered_ids(&client_fwd, &doc_id).await;
    let second = ordered_ids(&client_fwd, &doc_id).await;
    assert_eq!(first, second, "pagination must be stable across calls");

    // Reversed insertion order yields the same result — order comes from the
    // sort key, not the backend's return order.
    let reversed = ordered_ids(&client_rev, &doc_id).await;
    assert_eq!(
        first, reversed,
        "order must be independent of backend/insertion order"
    );

    // And that stable order is ascending by chunk_id.
    assert_eq!(first.len(), 2);
    let mut expected = first.clone();
    expected.sort();
    assert_eq!(first, expected, "tie should break by ascending chunk_id");
}
