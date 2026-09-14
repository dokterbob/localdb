//! `get_document` tool tests.

use serde_json::{json, Value};

use crate::harness::{
    call_tool, client_for, make_handler_with_one_store, make_handler_with_seeded_store, text_of,
};

/// T14: get_document by ID returns document metadata and text
#[tokio::test]
async fn test_get_document_by_id() {
    let (handler, doc_id, _) = make_handler_with_seeded_store().await;
    let client = client_for(handler).await;

    let result = call_tool(&client, "get_document", json!({ "id": doc_id }))
        .await
        .expect("get_document succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).expect("valid JSON in content");
    assert_eq!(parsed["resource_id"], doc_id);
    assert_eq!(parsed["uri"], "file:///docs/test.md");
    assert!(parsed.get("chunk_count").is_some());
    assert!(parsed.get("text").is_some());
    assert!(parsed.get("provenance").is_some());
    assert!(parsed.get("store").is_some());
}

/// T15: get_document with unknown ID → resource_not_found tool error
#[tokio::test]
async fn test_get_document_resource_not_found() {
    let (handler, _, _) = make_handler_with_seeded_store().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "get_document",
        json!({ "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }),
    )
    .await
    .expect("call succeeds at the protocol level");

    assert_eq!(result.is_error, Some(true), "should be a tool error");
    let error_text = text_of(&result);
    assert!(
        error_text.contains("resource_not_found"),
        "should report resource_not_found: {error_text}"
    );
}

/// get_document with no arguments at all: `id` is `#[serde(default)]` (see
/// args.rs's doc comment — a hard-required `id` would fail deserialization
/// for *any* omitted-`id` call, including a `uri`-only one, before the tool
/// body's more specific "uri not supported" guidance ever runs), so this
/// reaches `tools::tool_get_document`'s body as an empty `id` and returns
/// its usual tool-level `invalid_request` error, not a deserialization error.
#[tokio::test]
async fn test_get_document_no_args() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = call_tool(&client, "get_document", json!({}))
        .await
        .expect("empty id is a tool-level error, not a protocol error");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(
        text.contains("invalid_request"),
        "error should be invalid_request: {text}"
    );
    assert!(
        text.contains("must not be empty"),
        "error should mention 'id' must not be empty: {text}"
    );
}

/// get_document called with only `uri` (omitting `id` entirely, as a real
/// MCP client unaware of localdb's v1 id-only lookup might do) must still
/// reach the tool body's `uri`-specific guidance message, not a generic
/// deserialization error — this is the actual case `id`'s
/// `#[serde(default)]` (see args.rs) exists to preserve.
#[tokio::test]
async fn test_get_document_uri_only_gets_helpful_message() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = call_tool(
        &client,
        "get_document",
        json!({ "uri": "file:///docs/guide.md" }),
    )
    .await
    .expect("uri-only call is a tool-level error, not a protocol error");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(
        text.contains("uri-based get_document is not supported"),
        "error should point the caller at 'id' from a search result: {text}"
    );
}
