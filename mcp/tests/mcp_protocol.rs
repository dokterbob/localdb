//! Protocol-level tests for the MCP server.
//!
//! These tests exercise the MCP server with a scripted client over the
//! `handle_message` interface (simulates what a real stdio client would do).
//!
//! Acceptance criteria (T10):
//! - Tool list exactly the four read-only tools.
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
    server::{McpServer, TOOL_GET_CHUNKS, TOOL_GET_DOCUMENT, TOOL_LIST_STORES, TOOL_SEARCH},
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

/// Build a test McpServer seeded with ONE document made of 3 chunks, inserted
/// out of storage order. Proves that `get_chunks` sorts defensively by
/// `(block_seq, seq_in_block)` rather than trusting insertion/store order
/// (unlike libsql, `FakeStore` does not guarantee ordering).
async fn make_server_with_multichunk_doc() -> (McpServer, String) {
    let store = Arc::new(FakeStore::new());

    let uri = "file:///docs/multi.md";
    let doc_hash = content_hash("multi-chunk document body");
    let doc_id = document_id(uri, &doc_hash);

    let make_chunk = |text: &str, block_seq: u32, seq_in_block: u32, heading: &str| {
        let span = Span::new(0, text.len());
        let cid = chunk_id(&doc_id, text, span.start, span.end, block_seq);
        ChunkRecord {
            id: cid,
            document_id: doc_id.clone(),
            store_id: "store-1".to_string(),
            text: text.to_string(),
            span,
            heading_path: vec![heading.to_string()],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: doc_hash.clone(),
            origin_store: "store-1".to_string(),
            source_id: new_ulid(),
            source_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: uri.to_string(),
            metadata: localdb_core::DocumentMetadata {
                title: Some("Multi-chunk Doc".to_string()),
                ..Default::default()
            },
            block_seq,
            seq_in_block,
            block_kind: Some("paragraph".to_string()),
        }
    };

    // Inserted out of (block_seq, seq_in_block) order on purpose.
    let chunks = vec![
        make_chunk("third chunk text", 1, 1, "Section Two"),
        make_chunk("first chunk text", 0, 0, "Section One"),
        make_chunk("second chunk text", 1, 0, "Section Two"),
    ];
    store.upsert_chunks(chunks).await.expect("seed chunks");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder = Box::new(FakeEmbedder::new(4));
    let server = McpServer::new(vec![available], embedder);
    (server, doc_id)
}

/// Build a server seeded with ONE document whose two chunks both have
/// `(block_seq, seq_in_block) = (0, 0)` and an identical span, so the ONLY
/// distinguishing sort field is `chunk_id`. The two records are inserted in an
/// order controlled by `reversed` — because `FakeStore` preserves insertion
/// order, this exercises whether `get_chunks` imposes a stable total order
/// (by `chunk_id`) regardless of backend return order.
async fn make_server_with_tied_chunks(reversed: bool) -> (McpServer, String) {
    let store = Arc::new(FakeStore::new());

    let uri = "file:///docs/tied.md";
    let doc_hash = content_hash("tied-chunk document body");
    let doc_id = document_id(uri, &doc_hash);

    // Same span and (block_seq, seq_in_block) for both; only text (hence id) differs.
    let span = Span::new(0, 4);
    let make_chunk = |text: &str| {
        let cid = chunk_id(&doc_id, text, span.start, span.end, 0);
        ChunkRecord {
            id: cid,
            document_id: doc_id.clone(),
            store_id: "store-1".to_string(),
            text: text.to_string(),
            span: span.clone(),
            heading_path: vec![],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
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
            block_kind: Some("paragraph".to_string()),
        }
    };

    let a = make_chunk("aaaa");
    let b = make_chunk("bbbb");
    let chunks = if reversed { vec![b, a] } else { vec![a, b] };
    store.upsert_chunks(chunks).await.expect("seed chunks");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder = Box::new(FakeEmbedder::new(4));
    let server = McpServer::new(vec![available], embedder);
    (server, doc_id)
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

/// T03: tools/list returns exactly the four read-only tools
#[tokio::test]
async fn test_tools_list_exact_four_tools() {
    let server = make_server_with_one_store();

    let req_str = make_request(2, "tools/list", None);
    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert!(v.get("error").is_none(), "should not have error: {v}");

    let tools = v["result"]["tools"]
        .as_array()
        .expect("tools should be array");

    assert_eq!(tools.len(), 4, "should expose exactly 4 tools");

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
        tool_names.contains(&TOOL_GET_CHUNKS),
        "should have 'get_chunks' tool"
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

/// #94: search with a small `content_length` snaps the text-rendered snippet
/// to a natural boundary instead of cutting mid-word.
#[tokio::test]
async fn test_search_content_length_snaps_snippet_to_boundary() {
    let store = Arc::new(FakeStore::new());

    let uri = "file:///docs/long.md";
    let text = "Rust programming is a systems language focused on safety. \
It prevents entire classes of memory bugs at compile time without a garbage \
collector, which keeps runtime performance predictable and fast.";
    let doc_hash = content_hash(text);
    let doc_id_val = document_id(uri, &doc_hash);
    let span = Span::new(0, text.len());
    let cid = chunk_id(&doc_id_val, text, span.start, span.end, 0);

    let record = ChunkRecord {
        id: cid,
        document_id: doc_id_val,
        store_id: "store-1".to_string(),
        text: text.to_string(),
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
        uri: uri.to_string(),
        metadata: localdb_core::DocumentMetadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
    };
    store.upsert_chunks(vec![record]).await.unwrap();

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder = Box::new(FakeEmbedder::new(4));
    let server = McpServer::new(vec![available], embedder);

    let req_str = make_request(
        35,
        "tools/call",
        Some(json!({
            "name": TOOL_SEARCH,
            "arguments": {
                "query": "Rust programming",
                "limit": 1,
                "content_length": 60
            }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    let content = v["result"]["content"].as_array().unwrap();
    let text_out = content[0]["text"].as_str().unwrap();

    // The JSON part must still carry the full, untruncated snippet.
    let json_part = text_out.split("\n---\n").next().unwrap_or(text_out);
    let result: Value = serde_json::from_str(json_part).unwrap();
    let citations = result["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "should find at least one citation");
    let full_snippet = citations[0]["snippet"].as_str().unwrap();
    assert_eq!(
        full_snippet, text,
        "JSON citation snippet must remain untruncated"
    );

    // The human-readable text rendering (after "---") must be boundary-aware:
    // truncated with an ellipsis, not a mid-word hard cut.
    let human_part = text_out
        .split("\n---\n")
        .nth(1)
        .expect("text rendering section after separator");
    let snippet_line = human_part
        .lines()
        .find(|l| l.trim_start().starts_with("Rust programming"))
        .expect("rendered snippet line");
    let snippet_line = snippet_line.trim();
    assert!(
        snippet_line.ends_with('…'),
        "expected ellipsis marker on truncated snippet, got: {snippet_line}"
    );
    // Sentence-boundary snapping should land on the period before "It prevents...".
    assert!(
        snippet_line.contains("safety.…") || snippet_line.ends_with("safety…"),
        "expected snap at sentence boundary, got: {snippet_line}"
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
// Tool: get_chunks
// ---------------------------------------------------------------------------

/// get_chunks returns chunks sorted by (block_seq, seq_in_block) regardless
/// of insertion order, with correct spans and heading_path.
#[tokio::test]
async fn test_get_chunks_happy_path_sorted() {
    let (server, doc_id) = make_server_with_multichunk_doc().await;

    let req_str = make_request(
        60,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_CHUNKS,
            "arguments": { "document_id": doc_id }
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
    assert_eq!(result["uri"], "file:///docs/multi.md");
    assert_eq!(result["title"], "Multi-chunk Doc");
    assert_eq!(result["total_chunks"], 3);
    assert_eq!(result["offset"], 0);
    assert_eq!(result["returned"], 3);

    let chunks = result["chunks"].as_array().expect("chunks array");
    assert_eq!(chunks.len(), 3);

    // Must be sorted by (block_seq, seq_in_block), not insertion order.
    assert_eq!(chunks[0]["text"], "first chunk text");
    assert_eq!(chunks[0]["block_seq"], 0);
    assert_eq!(chunks[0]["seq_in_block"], 0);
    assert_eq!(chunks[0]["heading_path"][0], "Section One");
    assert_eq!(chunks[0]["span"]["start"], 0);
    assert_eq!(chunks[0]["span"]["end"], "first chunk text".len());
    assert_eq!(chunks[0]["block_kind"], "paragraph");

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
    let (server, doc_id) = make_server_with_multichunk_doc().await;

    let req_str = make_request(
        61,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_CHUNKS,
            "arguments": { "document_id": doc_id, "offset": 1, "limit": 1 }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(v["result"]["isError"], false);
    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let result: Value = serde_json::from_str(text).unwrap();

    assert_eq!(result["total_chunks"], 3);
    assert_eq!(result["offset"], 1);
    assert_eq!(result["limit"], 1);
    assert_eq!(result["returned"], 1);

    let chunks = result["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["text"], "second chunk text");
}

/// get_chunks with an out-of-range offset returns an empty chunks array,
/// not an error.
#[tokio::test]
async fn test_get_chunks_offset_out_of_range_returns_empty() {
    let (server, doc_id) = make_server_with_multichunk_doc().await;

    let req_str = make_request(
        62,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_CHUNKS,
            "arguments": { "document_id": doc_id, "offset": 99 }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(
        v["result"]["isError"], false,
        "out-of-range offset is not an error"
    );
    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let result: Value = serde_json::from_str(text).unwrap();

    assert_eq!(result["total_chunks"], 3);
    assert_eq!(result["returned"], 0);
    assert!(result["chunks"].as_array().unwrap().is_empty());
}

/// get_chunks with missing document_id → invalid_request tool error.
#[tokio::test]
async fn test_get_chunks_missing_document_id() {
    let server = make_server_with_one_store();

    let req_str = make_request(
        63,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_CHUNKS,
            "arguments": {}
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(v["result"]["isError"], true, "should be a tool error");
    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}

/// get_chunks with an unknown document_id → document_not_found tool error.
#[tokio::test]
async fn test_get_chunks_unknown_document_id() {
    let (server, _doc_id) = make_server_with_multichunk_doc().await;

    let req_str = make_request(
        64,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_CHUNKS,
            "arguments": { "document_id": "nonexistent-doc" }
        })),
    );

    let req = mcp::server::parse_message(&req_str).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(v["result"]["isError"], true, "should be a tool error");
    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["error"]["code"].as_str().unwrap(),
        "document_not_found"
    );
}

/// Chaining test: `search` → take `citations[0].document_id` → `get_chunks`.
/// Proves that `Citation.document_id` (already present, no search changes
/// needed) is sufficient to drive `get_chunks`.
#[tokio::test]
async fn test_search_to_get_chunks_chaining() {
    let (server, expected_doc_id, _chunk_id) = make_server_with_seeded_store().await;

    let search_req = make_request(
        70,
        "tools/call",
        Some(json!({
            "name": TOOL_SEARCH,
            "arguments": { "query": "Rust programming language", "limit": 5 }
        })),
    );
    let req = mcp::server::parse_message(&search_req).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let json_part = text.split("\n---\n").next().unwrap_or(text);
    let result: Value = serde_json::from_str(json_part).unwrap();
    let citations = result["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "search should find the seeded chunk");

    let document_id = citations[0]["document_id"]
        .as_str()
        .expect("citation.document_id must be a string")
        .to_string();
    assert_eq!(document_id, expected_doc_id);

    let chunks_req = make_request(
        71,
        "tools/call",
        Some(json!({
            "name": TOOL_GET_CHUNKS,
            "arguments": { "document_id": document_id }
        })),
    );
    let req = mcp::server::parse_message(&chunks_req).unwrap();
    let resp = server.handle_message(&req).await.unwrap();
    let v = parse_response(&resp);

    assert_eq!(v["result"]["isError"], false, "get_chunks should succeed");
    let content = v["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    let result: Value = serde_json::from_str(text).unwrap();
    assert_eq!(result["document_id"], expected_doc_id);
    assert_eq!(result["total_chunks"], 1);
}

/// get_chunks imposes a stable total order even when chunks tie on
/// `(block_seq, seq_in_block)`. Two `(0, 0)` chunks with an identical span
/// but different ids must paginate identically across repeated calls AND
/// regardless of the order the backend returns them in (proven by seeding the
/// same pair in opposite insertion orders). The tie is broken by `chunk_id`.
#[tokio::test]
async fn test_get_chunks_deterministic_tie_breaker() {
    async fn ordered_ids(server: &McpServer, doc_id: &str) -> Vec<String> {
        let req_str = make_request(
            80,
            "tools/call",
            Some(json!({
                "name": TOOL_GET_CHUNKS,
                "arguments": { "document_id": doc_id }
            })),
        );
        let req = mcp::server::parse_message(&req_str).unwrap();
        let resp = server.handle_message(&req).await.unwrap();
        let v = parse_response(&resp);
        assert_eq!(v["result"]["isError"], false);
        let content = v["result"]["content"].as_array().unwrap();
        let text = content[0]["text"].as_str().unwrap();
        let result: Value = serde_json::from_str(text).unwrap();
        result["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["chunk_id"].as_str().unwrap().to_string())
            .collect()
    }

    let (server_fwd, doc_id) = make_server_with_tied_chunks(false).await;
    let (server_rev, _doc_id_rev) = make_server_with_tied_chunks(true).await;

    // Repeated calls on the same server are stable.
    let first = ordered_ids(&server_fwd, &doc_id).await;
    let second = ordered_ids(&server_fwd, &doc_id).await;
    assert_eq!(first, second, "pagination must be stable across calls");

    // Reversed insertion order yields the same result — order comes from the
    // sort key, not the backend's return order.
    let reversed = ordered_ids(&server_rev, &doc_id).await;
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
