//! `list_stores` tool tests.

use serde_json::{json, Value};

use crate::harness::{call_tool, client_for, handler_with_stores, make_handler_with_one_store};

/// T07: list_stores returns all available stores
#[tokio::test]
async fn test_list_stores_returns_stores() {
    let client = client_for(make_handler_with_one_store()).await;

    let result = call_tool(&client, "list_stores", json!({}))
        .await
        .expect("list_stores succeeds");
    assert_ne!(result.is_error, Some(true), "should not be a tool error");

    let text = crate::harness::text_of(&result);
    let parsed: Value = serde_json::from_str(&text).expect("valid JSON in content");
    let stores = parsed["stores"].as_array().expect("stores array");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["name"], "test-store");
    assert_eq!(stores[0]["visibility"], "private");
    assert!(stores[0].get("chunk_count").is_some());
    assert!(stores[0].get("document_count").is_some());
}

/// T08: list_stores with empty stores returns empty list
#[tokio::test]
async fn test_list_stores_empty() {
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(4));
    let handler = handler_with_stores(vec![], embedder, false);
    let client = client_for(handler).await;

    let result = call_tool(&client, "list_stores", json!({}))
        .await
        .expect("list_stores succeeds");
    let text = crate::harness::text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["stores"].as_array().unwrap().len(), 0);
}
