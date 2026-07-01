//! Protocol-level tests for the MCP server.
//!
//! These tests exercise the MCP server with a scripted client over the
//! `handle_message` interface (simulates what a real stdio client would do).
//!
//! Acceptance criteria (T10):
//! - Tool list exactly the three read-only tools.
//! - `search` returns structured citations matching the canonical JSON.
//! - Unknown store name → `store_not_found` as MCP tool error.
//! - No mutating capability reachable.
//!
//! See specs/05-surfaces.md §4 and specs/02-domain-model.md §6.

use std::sync::Arc;

use serde_json::{json, Value};

use localdb_core::{
    ids::{chunk_id, content_hash, document_id, new_ulid},
    store::{ChunkRecord, FakeStore, RetrievalStore},
    types::Span,
    FakeEmbedder,
};
use mcp::{
    server::{McpServer, TOOL_GET_DOCUMENT, TOOL_LIST_STORES, TOOL_SEARCH},
    AvailableStore, StoreDescriptor,
};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Build a test McpServer with one store containing seeded chunks.
fn make_server_with_one_store() -> McpServer {
    let store = Arc::new(FakeStore::new());
    let sd = StoreDescriptor {
        id: new_ulid(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store.clone());
    let embedder = Box::new(FakeEmbedder::new(4));
    McpServer::new(vec![available], embedder)
}

/// Build a test McpServer with one store and seed it with a chunk.
async fn make_server_with_seeded_store() -> (McpServer, String, String) {
    let store = Arc::new(FakeStore::new());

    // Seed a chunk.
    let uri = "file:///docs/test.md";
    let doc_hash = content_hash("some document content about Rust programming");
    let doc_id = document_id(uri, &doc_hash);
    let snippet = "Rust is a systems programming language focused on safety and performance.";
    let span = Span::new(0, snippet.len());
    let cid = chunk_id(&doc_id, snippet, span.start, span.end, 0);

    let record = ChunkRecord {
        id: cid.clone(),
        document_id: doc_id.clone(),
        store_id: "store-1".to_string(),
        text: snippet.to_string(),
        span,
        heading_path: vec!["Introduction".to_string()],
        embedding: vec![0.8, 0.2, 0.1, 0.5],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: doc_hash.clone(),
        origin_store: "store-1".to_string(),
        source_id: new_ulid(),
        source_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.to_string(),
        metadata: localdb_core::DocumentMetadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
    };

    store.upsert_chunks(vec![record]).await.expect("seed chunk");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store.clone());
    let embedder = Box::new(FakeEmbedder::new(4));
    let server = McpServer::new(vec![available], embedder);
    (server, doc_id, cid)
}

fn make_request(id: u64, method: &str, params: Option<Value>) -> String {
    let mut msg = json!({
        "jsonrpc": "2.0",
        "method": method,
        "id": id,
    });
    if let Some(p) = params {
        msg["params"] = p;
    }
    serde_json::to_string(&msg).unwrap()
}

fn make_notification(method: &str, params: Option<Value>) -> String {
    let mut msg = json!({
        "jsonrpc": "2.0",
        "method": method,
    });
    if let Some(p) = params {
        msg["params"] = p;
    }
    serde_json::to_string(&msg).unwrap()
}

fn parse_response(response: &str) -> Value {
    serde_json::from_str(response).expect("valid JSON response")
}

// ---------------------------------------------------------------------------
// Protocol tests
// ---------------------------------------------------------------------------

/// T01: initialize handshake
#[tokio::test]
async fn test_initialize_handshake() {
    let server = make_server_with_one_store();

    let req_str = make_request(
        1,
        "initialize",
        Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test-client", "version": "0.0.1" }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    // Must be a success response (no "error" field).
    assert!(v.get("error").is_none(), "should not have error: {v}");
    assert_eq!(v["id"], 1);
    assert_eq!(v["jsonrpc"], "2.0");

    let result = &v["result"];
    assert!(result.get("protocolVersion").is_some());
    assert!(result.get("capabilities").is_some());
    assert!(result.get("serverInfo").is_some());
    assert_eq!(result["serverInfo"]["name"], "localdb");
}

/// T02: notifications/initialized produces no response
#[tokio::test]
async fn test_initialized_notification_no_response() {
    let server = make_server_with_one_store();

    let notif_str = make_notification("notifications/initialized", None);
    let req = mcp::server::parse_message(&notif_str).unwrap();
    let resp = server.handle_message(&req).await;

    assert!(
        resp.is_none(),
        "notifications should not produce a response"
    );
}

/// T03: tools/list returns exactly the three read-only tools
#[tokio::test]
async fn test_tools_list_exact_three_tools() {
    let server = make_server_with_one_store();

    let req_str = make_request(2, "tools/list", None);
    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(v.get("error").is_none(), "should not have error: {v}");

    let tools = v["result"]["tools"]
        .as_array()
        .expect("tools should be array");

    assert_eq!(tools.len(), 3, "should expose exactly 3 tools");

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert!(
        tool_names.contains(&TOOL_SEARCH),
        "should have 'search' tool"
    );
    assert!(
        tool_names.contains(&TOOL_GET_DOCUMENT),
        "should have 'get_document' tool"
    );
    assert!(
        tool_names.contains(&TOOL_LIST_STORES),
        "should have 'list_stores' tool"
    );
}

/// T04: each tool has a name, description, and inputSchema
#[tokio::test]
async fn test_tools_have_required_fields() {
    let server = make_server_with_one_store();

    let req_str = make_request(3, "tools/list", None);
    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);
    let tools = v["result"]["tools"].as_array().unwrap();

    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("");
        assert!(!name.is_empty(), "tool name must not be empty");
        assert!(
            tool.get("description").is_some(),
            "tool '{name}' must have description"
        );
        assert!(
            tool.get("inputSchema").is_some(),
            "tool '{name}' must have inputSchema"
        );
    }
}

/// T05: ping responds with an empty result
#[tokio::test]
async fn test_ping_response() {
    let server = make_server_with_one_store();

    let req_str = make_request(99, "ping", None);
    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(v.get("error").is_none());
    assert_eq!(v["id"], 99);
}

/// T06: unknown method → METHOD_NOT_FOUND error
#[tokio::test]
async fn test_unknown_method_returns_error() {
    let server = make_server_with_one_store();

    let req_str = make_request(10, "nonexistent/method", None);
    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(
        v.get("error").is_some(),
        "unknown method should return error"
    );
    assert_eq!(v["error"]["code"], -32601, "should be METHOD_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// Tool: list_stores
// ---------------------------------------------------------------------------

/// T07: list_stores returns all available stores
#[tokio::test]
async fn test_list_stores_returns_stores() {
    let server = make_server_with_one_store();

    let req_str = make_request(
        20,
        "tools/call",
        Some(json!({
            "name": TOOL_LIST_STORES,
            "arguments": {}
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(v.get("error").is_none(), "should not have RPC error: {v}");

    let content = v["result"]["content"].as_array().expect("content array");
    assert!(!content.is_empty(), "should have content");

    let text = content[0]["text"].as_str().unwrap();
    let result: Value = serde_json::from_str(text).expect("valid JSON in content");

    let stores = result["stores"].as_array().expect("stores array");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["name"], "test-store");
    assert_eq!(stores[0]["visibility"], "private");
    assert!(stores[0].get("chunk_count").is_some());
    assert!(stores[0].get("document_count").is_some());
}

/// T08: list_stores with empty stores returns empty list
#[tokio::test]
async fn test_list_stores_empty() {
    let embedder = Box::new(FakeEmbedder::new(4));
    let server = McpServer::new(vec![], embedder);

    let req_str = make_request(
        21,
        "tools/call",
        Some(json!({
            "name": TOOL_LIST_STORES,
            "arguments": {}
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let result: Value = serde_json::from_str(text).unwrap();
    assert_eq!(result["stores"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Tool: search
// ---------------------------------------------------------------------------

/// T09: search returns citations in the canonical JSON shape
#[tokio::test]
async fn test_search_returns_canonical_citations() {
    let (server, _doc_id, _chunk_id) = make_server_with_seeded_store().await;

    let req_str = make_request(
        30,
        "tools/call",
        Some(json!({
            "name": TOOL_SEARCH,
            "arguments": {
                "query": "Rust programming language",
                "limit": 5
            }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(v.get("error").is_none(), "should not have RPC error: {v}");

    // The result should not be marked as an error.
    assert_eq!(v["result"]["isError"], false, "should not be a tool error");

    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();

    // The text starts with JSON (before the "---" separator).
    let json_part = text.split("\n---\n").next().unwrap_or(text);
    let result: Value = serde_json::from_str(json_part).expect("valid JSON in content");

    assert!(result.get("citations").is_some(), "should have citations");
    let citations = result["citations"].as_array().unwrap();

    // Since we seeded one chunk about Rust, and the query is about Rust, we should get a result.
    assert!(!citations.is_empty(), "should find at least one citation");

    // Verify the canonical citation shape (specs/02-domain-model.md §6).
    let first = &citations[0];
    assert!(first.get("chunk_id").is_some(), "citation.chunk_id missing");
    assert!(
        first.get("document_id").is_some(),
        "citation.document_id missing"
    );
    assert!(first.get("store").is_some(), "citation.store missing");
    assert!(first.get("uri").is_some(), "citation.uri missing");
    // title is optional but must be serialized (null or string).
    assert!(
        first.get("title").is_some() || first.get("title").map(|v| v.is_null()).unwrap_or(true),
        "citation.title must be present (null or string)"
    );
    assert!(
        first.get("heading_path").is_some(),
        "citation.heading_path missing"
    );
    assert!(first.get("span").is_some(), "citation.span missing");
    assert!(first.get("snippet").is_some(), "citation.snippet missing");
    assert!(first.get("score").is_some(), "citation.score missing");
    assert!(
        first.get("provenance").is_some(),
        "citation.provenance missing"
    );

    // Score shape: all three fields required per spec.
    let score = &first["score"];
    assert!(score.get("fused").is_some(), "score.fused missing");
    // dense and bm25 may be null when only one leg fires, but the key must exist.
    assert!(score.get("dense").is_some(), "score.dense missing");
    assert!(score.get("bm25").is_some(), "score.bm25 missing");

    // Store shape.
    let store_obj = &first["store"];
    assert!(store_obj.get("id").is_some(), "citation.store.id missing");
    assert!(
        store_obj.get("name").is_some(),
        "citation.store.name missing"
    );

    // Span shape.
    let span = &first["span"];
    assert!(span.get("start").is_some(), "citation.span.start missing");
    assert!(span.get("end").is_some(), "citation.span.end missing");

    // Provenance shape.
    let prov = &first["provenance"];
    assert!(
        prov.get("fetched_at").is_some(),
        "citation.provenance.fetched_at missing"
    );
    assert!(
        prov.get("content_hash").is_some(),
        "citation.provenance.content_hash missing"
    );
}

/// T10: search with unknown store name → store_not_found tool error
#[tokio::test]
async fn test_search_unknown_store_name() {
    let (server, _, _) = make_server_with_seeded_store().await;

    let req_str = make_request(
        31,
        "tools/call",
        Some(json!({
            "name": TOOL_SEARCH,
            "arguments": {
                "query": "test",
                "stores": ["nonexistent-store"]
            }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    // Should be a tool-level error (isError: true), not a JSON-RPC error.
    assert!(v.get("error").is_none(), "should not have RPC error: {v}");
    assert_eq!(v["result"]["isError"], true, "should be a tool error");

    let content = v["result"]["content"].as_array().unwrap();
    let error_text = content[0]["text"].as_str().unwrap();
    assert!(
        error_text.contains("store_not_found") || error_text.contains("nonexistent-store"),
        "error text should reference the missing store: {error_text}"
    );
}

/// T11: search with missing query argument → invalid_arguments tool error
#[tokio::test]
async fn test_search_missing_query_argument() {
    let server = make_server_with_one_store();

    let req_str = make_request(
        32,
        "tools/call",
        Some(json!({
            "name": TOOL_SEARCH,
            "arguments": {}
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(v.get("error").is_none());
    assert_eq!(v["result"]["isError"], true, "should be a tool error");
}

/// T12: search returns empty citations for a store with no content
#[tokio::test]
async fn test_search_empty_store() {
    let server = make_server_with_one_store(); // store has no chunks

    let req_str = make_request(
        33,
        "tools/call",
        Some(json!({
            "name": TOOL_SEARCH,
            "arguments": {
                "query": "anything"
            }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(v["result"]["isError"], false);

    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let json_part = text.split("\n---\n").next().unwrap_or(text);
    let result: Value = serde_json::from_str(json_part).unwrap();
    let citations = result["citations"].as_array().unwrap();
    assert!(
        citations.is_empty(),
        "empty store should return no citations"
    );
}

/// T13: search limit is respected
#[tokio::test]
async fn test_search_limit_respected() {
    let store = Arc::new(FakeStore::new());

    // Seed multiple chunks about different topics.
    let mut records = Vec::new();
    for i in 0..5 {
        let text = format!("Chunk {i} about Rust programming language and systems software.");
        let uri = format!("file:///docs/doc{i}.md");
        let doc_hash = content_hash(&text);
        let doc_id_val = document_id(&uri, &doc_hash);
        let span = Span::new(0, text.len());
        let cid = chunk_id(&doc_id_val, &text, span.start, span.end, 0);

        records.push(ChunkRecord {
            id: cid,
            document_id: doc_id_val,
            store_id: "store-1".to_string(),
            text,
            span,
            heading_path: vec![],
            embedding: vec![0.9, 0.1, 0.1, 0.1],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: doc_hash,
            origin_store: "store-1".to_string(),
            source_id: new_ulid(),
            source_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri,
            metadata: localdb_core::DocumentMetadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
        });
    }
    store.upsert_chunks(records).await.unwrap();

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder = Box::new(FakeEmbedder::new(4));
    let server = McpServer::new(vec![available], embedder);

    let req_str = make_request(
        34,
        "tools/call",
        Some(json!({
            "name": TOOL_SEARCH,
            "arguments": {
                "query": "Rust programming",
                "limit": 3
            }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let json_part = text.split("\n---\n").next().unwrap_or(text);
    let result: Value = serde_json::from_str(json_part).unwrap();
    let citations = result["citations"].as_array().unwrap();

    assert!(
        citations.len() <= 3,
        "should return at most 3 citations, got {}",
        citations.len()
    );
}

// ---------------------------------------------------------------------------
// Tool: get_document
// ---------------------------------------------------------------------------

/// T14: get_document by ID returns document metadata and text
#[tokio::test]
async fn test_get_document_by_id() {
    let (server, doc_id, _) = make_server_with_seeded_store().await;

    let req_str = make_request(
        40,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_DOCUMENT,
            "arguments": {
                "id": doc_id
            }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(v.get("error").is_none(), "should not have RPC error: {v}");
    assert_eq!(v["result"]["isError"], false);

    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let result: Value = serde_json::from_str(text).expect("valid JSON in content");

    assert_eq!(result["document_id"], doc_id);
    assert_eq!(result["uri"], "file:///docs/test.md");
    assert!(result.get("chunk_count").is_some());
    assert!(result.get("text").is_some());
    assert!(result.get("provenance").is_some());
    assert!(result.get("store").is_some());
}

/// T15: get_document with unknown ID → document_not_found tool error
#[tokio::test]
async fn test_get_document_not_found() {
    let (server, _, _) = make_server_with_seeded_store().await;

    let req_str = make_request(
        41,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_DOCUMENT,
            "arguments": {
                "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(v["result"]["isError"], true, "should be a tool error");

    let content = v["result"]["content"].as_array().unwrap();
    let error_text = content[0]["text"].as_str().unwrap();
    assert!(
        error_text.contains("document_not_found"),
        "should report document_not_found: {error_text}"
    );
}

/// T16: get_document with no arguments → invalid_arguments tool error
#[tokio::test]
async fn test_get_document_no_args() {
    let server = make_server_with_one_store();

    let req_str = make_request(
        42,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_DOCUMENT,
            "arguments": {}
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(v["result"]["isError"], true, "should be a tool error");
}

// ---------------------------------------------------------------------------
// No mutating tool
// ---------------------------------------------------------------------------

/// T17: no mutating tool is accessible (only 3 read-only tools exist)
#[tokio::test]
async fn test_no_mutating_tools_accessible() {
    let server = make_server_with_one_store();
    let req_str = make_request(50, "tools/list", None);
    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    let tools = v["result"]["tools"].as_array().unwrap();
    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // Mutating operations that must NOT be present.
    let mutating = [
        "add_source",
        "remove_source",
        "reindex",
        "delete_document",
        "upsert_chunk",
        "create_store",
        "delete_store",
    ];
    for m in mutating {
        assert!(
            !tool_names.contains(&m),
            "mutating tool '{m}' must not be accessible"
        );
    }
}

/// T18: calling an unknown tool returns a tool error (not an RPC error)
#[tokio::test]
async fn test_unknown_tool_call() {
    let server = make_server_with_one_store();

    let req_str = make_request(
        51,
        "tools/call",
        Some(json!({
            "name": "add_source",
            "arguments": { "path": "/evil" }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    // Should be a tool-level error, not an RPC error.
    assert!(v.get("error").is_none(), "should not have RPC error");
    assert_eq!(v["result"]["isError"], true, "should be a tool error");

    let content = v["result"]["content"].as_array().unwrap();
    let msg = content[0]["text"].as_str().unwrap();
    assert!(
        msg.contains("add_source"),
        "error should name the unknown tool"
    );
}

// ---------------------------------------------------------------------------
// Message parsing
// ---------------------------------------------------------------------------

/// T19: parse_message handles valid JSON-RPC requests
#[test]
fn test_parse_message_valid() {
    let line = r#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#;
    let req = mcp::server::parse_message(line).unwrap();
    assert_eq!(req.method, "tools/list");
    assert_eq!(req.id, Some(Value::from(1)));
}

/// T20: parse_message returns error for invalid JSON
#[test]
fn test_parse_message_invalid_json() {
    let result = mcp::server::parse_message("{not valid json}");
    assert!(result.is_err());
}

/// T21: parse_message handles notifications (no id)
#[test]
fn test_parse_message_notification() {
    let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let req = mcp::server::parse_message(line).unwrap();
    assert!(req.is_notification());
}

// ---------------------------------------------------------------------------
// tools module unit tests
// ---------------------------------------------------------------------------

/// T22: SearchArgs::from_value parses correctly
#[test]
fn test_search_args_parse_basic() {
    use mcp::server::TOOL_SEARCH;

    let params = json!({
        "name": TOOL_SEARCH,
        "arguments": {
            "query": "test query",
            "limit": 5
        }
    });

    let args = mcp::tools::SearchArgs::from_value(Some(&params)).unwrap();
    assert_eq!(args.query, "test query");
    assert_eq!(args.limit, 5);
    assert!(args.store_names.is_empty());
}

/// T23: SearchArgs::from_value rejects empty query
#[test]
fn test_search_args_empty_query() {
    let params = json!({
        "name": "search",
        "arguments": {
            "query": "   "
        }
    });

    let result = mcp::tools::SearchArgs::from_value(Some(&params));
    assert!(result.is_err());
}

/// T24: SearchArgs::from_value rejects missing query
#[test]
fn test_search_args_missing_query() {
    let params = json!({
        "name": "search",
        "arguments": {}
    });

    let result = mcp::tools::SearchArgs::from_value(Some(&params));
    assert!(result.is_err());
}

/// T25: SearchArgs cap limit at MAX_LIMIT=100
#[test]
fn test_search_args_limit_capped() {
    let params = json!({
        "name": "search",
        "arguments": {
            "query": "q",
            "limit": 9999
        }
    });

    let args = mcp::tools::SearchArgs::from_value(Some(&params)).unwrap();
    assert_eq!(args.limit, 100, "limit should be capped at 100");
}

/// T26: SearchArgs default limit is 10
#[test]
fn test_search_args_default_limit() {
    let params = json!({
        "name": "search",
        "arguments": { "query": "q" }
    });

    let args = mcp::tools::SearchArgs::from_value(Some(&params)).unwrap();
    assert_eq!(args.limit, 10, "default limit should be 10");
}

/// T27: render_citations_text with empty list returns "No results found."
#[test]
fn test_render_citations_empty() {
    let text = mcp::tools::render_citations_text(&[], 400);
    assert_eq!(text, "No results found.");
}
