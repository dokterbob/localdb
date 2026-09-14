//! `get_chunks` anchor-relative pagination tests (#146): `anchor_chunk_id` /
//! `anchor_block_seq`, centered windows, edge clamping, and mutual
//! exclusivity with `offset`.

use serde_json::{json, Value};

use crate::harness::{
    call_tool, client_for, make_handler_with_block_seq_gaps, make_handler_with_multichunk_doc,
    make_handler_with_sequential_chunks, text_of,
};

/// Reproduces the spec's worked example verbatim (specs/05-surfaces.md
/// §4.1): 20 chunks (one per block, `block_seq` 0-19), `anchor_chunk_id` at
/// `block_seq = 10`, `limit: 5` -> centered window covering `block_seq`
/// 8-12, `offset: 8`, and the anchor as the 3rd of 5 returned chunks
/// (`anchor_index: 2`).
#[tokio::test]
async fn test_get_chunks_anchor_chunk_id_centered_window_spec_example() {
    let (handler, doc_id, ids) = make_handler_with_sequential_chunks(20).await;
    let client = client_for(handler).await;

    let anchor_id = ids[10].clone();
    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_chunk_id": anchor_id, "limit": 5 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["total_chunks"], 20);
    assert_eq!(parsed["offset"], 8);
    assert_eq!(parsed["limit"], 5);
    assert_eq!(parsed["returned"], 5);
    assert_eq!(parsed["anchor_index"], 2);

    let chunks = parsed["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 5);
    for (i, expected_block_seq) in (8i32..=12).enumerate() {
        assert_eq!(chunks[i]["block_seq"], expected_block_seq);
    }
    assert_eq!(chunks[2]["chunk_id"], anchor_id);
}

/// The same anchor resolved via `anchor_block_seq` instead of
/// `anchor_chunk_id` must produce an identical window (same `offset` and
/// `anchor_index`, and the anchor chunk at the same position).
#[tokio::test]
async fn test_get_chunks_anchor_block_seq_centered_window_matches_chunk_id() {
    let (handler, doc_id, ids) = make_handler_with_sequential_chunks(20).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_block_seq": 10, "limit": 5 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["offset"], 8);
    assert_eq!(parsed["anchor_index"], 2);
    assert_eq!(parsed["chunks"][2]["chunk_id"], ids[10]);
}

/// The spec's second worked example: the same anchor with `limit: 30`
/// against the 20-chunk resource clamps to the whole list: `offset: 0`,
/// `returned: 20`, `anchor_index: 10`.
#[tokio::test]
async fn test_get_chunks_anchor_limit_greater_than_total_clamps_to_whole_list() {
    let (handler, doc_id, _ids) = make_handler_with_sequential_chunks(20).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_block_seq": 10, "limit": 30 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["total_chunks"], 20);
    assert_eq!(parsed["offset"], 0);
    assert_eq!(parsed["returned"], 20);
    assert_eq!(parsed["anchor_index"], 10);
}

/// Clamping near the start: an anchor at `block_seq = 1` with `limit: 5`
/// cannot center (a centered window would need `offset: -1`) — the window
/// shifts toward the interior and clamps at `offset: 0`, so the anchor
/// sits at `anchor_index: 1`, not the fully-centered `2`.
#[tokio::test]
async fn test_get_chunks_anchor_clamps_at_start() {
    let (handler, doc_id, _ids) = make_handler_with_sequential_chunks(20).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_block_seq": 1, "limit": 5 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["offset"], 0, "window must clamp at the start");
    assert_eq!(
        parsed["returned"], 5,
        "window must stay full-sized even near the edge"
    );
    assert_eq!(parsed["anchor_index"], 1);
}

/// Clamping near the end: an anchor at `block_seq = 18` (index 18 of 20)
/// with `limit: 5` would need `offset: 16` to center, but `16 + 5 = 21 >
/// 20` — clamps to `offset: 15`, so the anchor sits at `anchor_index: 3`,
/// not the fully-centered `2`.
#[tokio::test]
async fn test_get_chunks_anchor_clamps_at_end() {
    let (handler, doc_id, _ids) = make_handler_with_sequential_chunks(20).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_block_seq": 18, "limit": 5 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["offset"], 15, "window must clamp at the end");
    assert_eq!(
        parsed["returned"], 5,
        "window must stay full-sized even near the edge"
    );
    assert_eq!(parsed["anchor_index"], 3);
}

/// `anchor_chunk_id` set to an id absent from the resource -> `chunk_not_found`.
#[tokio::test]
async fn test_get_chunks_anchor_chunk_id_unknown_is_chunk_not_found() {
    let (handler, doc_id, _ids) = make_handler_with_sequential_chunks(5).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_chunk_id": "does-not-exist" }),
    )
    .await
    .expect("call succeeds at the protocol level");
    assert_eq!(result.is_error, Some(true));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "chunk_not_found");
}

/// `anchor_block_seq` past every block in the resource -> `chunk_not_found`.
#[tokio::test]
async fn test_get_chunks_anchor_block_seq_past_end_is_chunk_not_found() {
    let (handler, doc_id, _ids) = make_handler_with_sequential_chunks(5).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_block_seq": 100 }),
    )
    .await
    .expect("call succeeds at the protocol level");
    assert_eq!(result.is_error, Some(true));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "chunk_not_found");
}

/// `anchor_block_seq` lower-bound resolution and tie-break: block seqs
/// present are {0, 5 (x3 chunks), 10}. An exact `anchor_block_seq: 5` must
/// resolve to the `seq_in_block = 0` chunk at that block (not one of its
/// two siblings) — the tie-break rule. An `anchor_block_seq: 1` (absent)
/// must resolve via lower-bound to the next block_seq present (5's first
/// chunk), not the nearest chunk by any other measure.
#[tokio::test]
async fn test_get_chunks_anchor_block_seq_lower_bound_and_tie_break() {
    let (handler, doc_id) = make_handler_with_block_seq_gaps().await;
    let client = client_for(handler).await;

    // Exact match on a block_seq with 3 chunks: must tie-break to seq_in_block 0.
    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_block_seq": 5, "limit": 3 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["total_chunks"], 5);
    let chunks = parsed["chunks"].as_array().unwrap();
    let anchor_idx = parsed["anchor_index"].as_u64().unwrap() as usize;
    assert_eq!(
        chunks[anchor_idx]["text"], "b5-0",
        "tie-break must pick the lowest seq_in_block at block_seq 5"
    );
    assert_eq!(chunks[anchor_idx]["seq_in_block"], 0);

    // Lower-bound: block_seq 1 doesn't exist -> resolves to block_seq 5's
    // first chunk (the next block_seq present).
    let result2 = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_block_seq": 1, "limit": 3 }),
    )
    .await
    .expect("get_chunks succeeds");
    assert_eq!(result2.is_error, Some(false));
    let text2 = text_of(&result2);
    let parsed2: Value = serde_json::from_str(&text2).unwrap();
    let chunks2 = parsed2["chunks"].as_array().unwrap();
    let anchor_idx2 = parsed2["anchor_index"].as_u64().unwrap() as usize;
    assert_eq!(chunks2[anchor_idx2]["text"], "b5-0");
}

/// Plain-`offset` (non-anchor) requests must carry `anchor_index: null`.
#[tokio::test]
async fn test_get_chunks_anchor_index_null_in_offset_mode() {
    let (handler, doc_id) = make_handler_with_multichunk_doc().await;
    let client = client_for(handler).await;

    let result = call_tool(&client, "get_chunks", json!({ "resource_id": doc_id }))
        .await
        .expect("get_chunks succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert!(
        parsed["anchor_index"].is_null(),
        "anchor_index must be null in plain-offset mode"
    );
}

/// `offset` + `anchor_chunk_id` together violates mutual exclusivity ->
/// tool-level `invalid_request` error.
#[tokio::test]
async fn test_get_chunks_offset_and_anchor_chunk_id_mutually_exclusive() {
    let (handler, doc_id, ids) = make_handler_with_sequential_chunks(5).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "offset": 1, "anchor_chunk_id": ids[2] }),
    )
    .await
    .expect("call succeeds at the protocol level");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}

/// `offset` + `anchor_block_seq` together violates mutual exclusivity ->
/// tool-level `invalid_request` error.
#[tokio::test]
async fn test_get_chunks_offset_and_anchor_block_seq_mutually_exclusive() {
    let (handler, doc_id, _ids) = make_handler_with_sequential_chunks(5).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "offset": 1, "anchor_block_seq": 2 }),
    )
    .await
    .expect("call succeeds at the protocol level");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}

/// `anchor_chunk_id` + `anchor_block_seq` together violates mutual
/// exclusivity -> tool-level `invalid_request` error.
#[tokio::test]
async fn test_get_chunks_anchor_chunk_id_and_anchor_block_seq_mutually_exclusive() {
    let (handler, doc_id, ids) = make_handler_with_sequential_chunks(5).await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_chunks",
        json!({ "resource_id": doc_id, "anchor_chunk_id": ids[2], "anchor_block_seq": 2 }),
    )
    .await
    .expect("call succeeds at the protocol level");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}
