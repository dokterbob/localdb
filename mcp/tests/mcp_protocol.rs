//! Protocol-level tests for the MCP server.
//!
//! These tests drive `McpHandler` over a real `rmcp` client/server pair
//! connected by an in-memory `tokio::io::duplex` — the same transport shape
//! a real stdio client would see, minus the OS pipe.
//!
//! Acceptance criteria (T10, carried over from the pre-rmcp suite):
//! - Tool list exactly the four read-only tools.
//! - `search` returns structured citations matching the canonical JSON.
//! - Unknown store name → `store_not_found` as MCP tool error.
//! - No mutating capability reachable.
//!
//! Two-tier error model (new in the rmcp migration, see `mcp/src/lib.rs` for
//! the full writeup — verified empirically here, not assumed):
//! - **Protocol-level** (`Err(ServiceError::McpError)`, `ErrorCode::INVALID_PARAMS`):
//!   only an unregistered tool *name* (`test_unknown_tool_call`).
//! - **Tool-level** (`Ok(CallToolResult { is_error: Some(true), .. })`):
//!   everything else — including a missing/wrong-typed *required* argument.
//!   One might expect that to be a protocol-level error since `Parameters<T>`
//!   deserialization itself produces an `ErrorData::invalid_params`, but
//!   rmcp 1.8.0's `ToolRouter::call` downgrades any such error to a tool
//!   result via `into_tool_argument_error` (see `assert_deserialization_error`
//!   below and `test_search_missing_query_argument` /
//!   `test_get_document_no_args` / `test_get_chunks_missing_resource_id`).
//!
//! See specs/05-surfaces.md §4 and specs/02-domain-model.md §6.

use serde_json::{json, Value};

use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ErrorCode},
    service::{RoleClient, RunningService, ServiceError},
    ServiceExt,
};

use localdb_core::{
    ids::{chunk_id, content_hash, new_ulid, resource_id},
    store::{ChunkRecord, FakeStore, RetrievalStore},
    types::Span,
    FakeEmbedder,
};
use mcp::{handler::McpHandler, AvailableStore, StoreDescriptor};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// Serve `handler` on one half of an in-memory duplex pipe and connect a
/// trivial (no-op) client to the other half — the same shape a real stdio
/// MCP client/server pair has, without an OS pipe.
async fn client_for(handler: McpHandler) -> RunningService<RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        match handler.serve(server_transport).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => panic!("server failed to initialize: {e}"),
        }
    });
    ().serve(client_transport)
        .await
        .expect("client should connect")
}

/// Call `name` with `arguments` (a JSON object) and return the raw result.
async fn call_tool(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    arguments: Value,
) -> Result<CallToolResult, ServiceError> {
    let args = arguments
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args))
        .await
}

/// Extract the text of the first content item of a `CallToolResult`.
fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("expected a text content item")
}

/// Assert `result` is a tool-level "failed to deserialize parameters" error
/// (rmcp's `ToolRouter::call` downgrades `Parameters<T>` deserialization
/// failures — including a missing required field — from the protocol-level
/// `ErrorData::invalid_params` that `Parameters<T>`'s extractor itself
/// produces into a tool-level `CallToolResult`, via
/// `into_tool_argument_error` in `rmcp::handler::server::router::tool`; see
/// the `mcp/src/lib.rs` doc comment for the full two-tier model as verified
/// against rmcp 1.8.0). Returns the error message.
fn assert_deserialization_error(result: Result<CallToolResult, ServiceError>) -> String {
    let result = result.expect("deserialization failures are tool-level, not protocol-level");
    assert_eq!(result.is_error, Some(true), "should be a tool error");
    let text = text_of(&result);
    assert!(
        text.starts_with("failed to deserialize parameters:"),
        "expected a parameter-deserialization error, got: {text}"
    );
    text
}

/// Build a handler with one empty store.
fn make_handler_with_one_store() -> McpHandler {
    let store = std::sync::Arc::new(FakeStore::new());
    let sd = StoreDescriptor {
        id: new_ulid(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    McpHandler::new(vec![available], embedder, false)
}

/// Build a handler with one store seeded with a chunk.
async fn make_handler_with_seeded_store() -> (McpHandler, String, String) {
    let store = std::sync::Arc::new(FakeStore::new());

    let uri = "file:///docs/test.md";
    let doc_hash = content_hash("some document content about Rust programming");
    let doc_id = resource_id(uri, &doc_hash);
    let snippet = "Rust is a systems programming language focused on safety and performance.";
    let span = Span::new(0, snippet.len());
    let cid = chunk_id(&doc_id, 0, snippet, 0);

    let record = ChunkRecord {
        id: cid.clone(),
        resource_id: doc_id.clone(),
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
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.to_string(),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        // Paginated source (#103): the MCP surface must carry this through to
        // citation.block.page with no surface-crate code change.
        page: Some(4),
        window_block_seqs: vec![],
    };

    store.upsert_chunks(vec![record]).await.expect("seed chunk");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![available], embedder, false);
    (handler, doc_id, cid)
}

/// Build a handler seeded with ONE document made of 3 chunks, inserted out
/// of storage order. Proves that `get_chunks` sorts defensively by
/// `(block_seq, seq_in_block)` rather than trusting insertion/store order
/// (unlike libsql, `FakeStore` does not guarantee ordering).
async fn make_handler_with_multichunk_doc() -> (McpHandler, String) {
    let store = std::sync::Arc::new(FakeStore::new());

    let uri = "file:///docs/multi.md";
    let doc_hash = content_hash("multi-chunk document body");
    let doc_id = resource_id(uri, &doc_hash);

    let make_chunk = |text: &str, block_seq: u32, seq_in_block: u32, heading: &str| {
        let span = Span::new(0, text.len());
        let cid = chunk_id(&doc_id, block_seq, text, seq_in_block);
        ChunkRecord {
            id: cid,
            resource_id: doc_id.clone(),
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
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: uri.to_string(),
            metadata: localdb_core::metadata::Metadata::Document(
                localdb_core::metadata::DocumentMetadata {
                    dublin_core: localdb_core::metadata::DublinCoreMetadata {
                        title: Some("Multi-chunk Doc".to_string()),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            block_seq,
            seq_in_block,
            block_kind: Some("text".to_string()),
            page: None,
            window_block_seqs: vec![],
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
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![available], embedder, false);
    (handler, doc_id)
}

/// Build a handler seeded with ONE document whose two chunks both have
/// `(block_seq, seq_in_block) = (0, 0)` and an identical span, so the ONLY
/// distinguishing sort field is `chunk_id`. The two records are inserted in
/// an order controlled by `reversed` — because `FakeStore` preserves
/// insertion order, this exercises whether `get_chunks` imposes a stable
/// total order (by `chunk_id`) regardless of backend return order.
async fn make_handler_with_tied_chunks(reversed: bool) -> (McpHandler, String) {
    let store = std::sync::Arc::new(FakeStore::new());

    let uri = "file:///docs/tied.md";
    let doc_hash = content_hash("tied-chunk document body");
    let doc_id = resource_id(uri, &doc_hash);

    // Same span and (block_seq, seq_in_block) for both; only text (hence id) differs.
    let span = Span::new(0, 4);
    let make_chunk = |text: &str| {
        let cid = chunk_id(&doc_id, 0, text, 0);
        ChunkRecord {
            id: cid,
            resource_id: doc_id.clone(),
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
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: uri.to_string(),
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: Some("text".to_string()),
            page: None,
            window_block_seqs: vec![],
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
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![available], embedder, false);
    (handler, doc_id)
}

/// Build a handler seeded with ONE document made of `count` chunks, one per
/// block (`block_seq` 0..count, `seq_in_block` 0) — mirrors the shape of the
/// spec's worked anchor-pagination example (specs/05-surfaces.md §4.1: 20
/// chunks, one chunk per block). Returns the handler, the resource id, and
/// the chunk ids in `(block_seq, seq_in_block)` order (index == block_seq).
async fn make_handler_with_sequential_chunks(count: u32) -> (McpHandler, String, Vec<String>) {
    let store = std::sync::Arc::new(FakeStore::new());

    let uri = "file:///docs/sequential.md";
    let doc_hash = content_hash("sequential document body");
    let doc_id = resource_id(uri, &doc_hash);

    let mut chunks = Vec::new();
    let mut ids = Vec::new();
    for block_seq in 0..count {
        let text = format!("chunk body {block_seq}");
        let cid = chunk_id(&doc_id, block_seq, &text, 0);
        ids.push(cid.clone());
        chunks.push(ChunkRecord {
            id: cid,
            resource_id: doc_id.clone(),
            store_id: "store-1".to_string(),
            text: text.clone(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: doc_hash.clone(),
            origin_store: "store-1".to_string(),
            source_id: new_ulid(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: uri.to_string(),
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq,
            seq_in_block: 0,
            block_kind: Some("text".to_string()),
            page: None,
            window_block_seqs: vec![],
        });
    }
    store.upsert_chunks(chunks).await.expect("seed chunks");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![available], embedder, false);
    (handler, doc_id, ids)
}

/// Build a handler seeded with ONE document with a gap in `block_seq` and a
/// block holding multiple chunks, for `anchor_block_seq` lower-bound and
/// tie-break tests (#146): `block_seq` 0 (one chunk), `block_seq` 5 (three
/// chunks, `seq_in_block` 0/1/2, inserted out of order), `block_seq` 10 (one
/// chunk).
async fn make_handler_with_block_seq_gaps() -> (McpHandler, String) {
    let store = std::sync::Arc::new(FakeStore::new());
    let uri = "file:///docs/gaps.md";
    let doc_hash = content_hash("gapped document body");
    let doc_id = resource_id(uri, &doc_hash);

    let make_chunk = |text: &str, block_seq: u32, seq_in_block: u32| {
        let cid = chunk_id(&doc_id, block_seq, text, seq_in_block);
        ChunkRecord {
            id: cid,
            resource_id: doc_id.clone(),
            store_id: "store-1".to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: doc_hash.clone(),
            origin_store: "store-1".to_string(),
            source_id: new_ulid(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: uri.to_string(),
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq,
            seq_in_block,
            block_kind: Some("text".to_string()),
            page: None,
            window_block_seqs: vec![],
        }
    };

    let chunks = vec![
        make_chunk("b0", 0, 0),
        make_chunk("b5-2", 5, 2),
        make_chunk("b5-0", 5, 0),
        make_chunk("b5-1", 5, 1),
        make_chunk("b10", 10, 0),
    ];
    store.upsert_chunks(chunks).await.expect("seed chunks");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![available], embedder, false);
    (handler, doc_id)
}

// ---------------------------------------------------------------------------
// tools/list
// ---------------------------------------------------------------------------

/// T03: tools/list returns exactly the four read-only tools
#[tokio::test]
async fn test_tools_list_exact_four_tools() {
    let client = client_for(make_handler_with_one_store()).await;

    let result = client.list_tools(None).await.expect("list_tools succeeds");
    assert_eq!(result.tools.len(), 4, "should expose exactly 4 tools");

    let tool_names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(tool_names.contains(&"search"), "should have 'search' tool");
    assert!(
        tool_names.contains(&"get_document"),
        "should have 'get_document' tool"
    );
    assert!(
        tool_names.contains(&"get_chunks"),
        "should have 'get_chunks' tool"
    );
    assert!(
        tool_names.contains(&"list_stores"),
        "should have 'list_stores' tool"
    );
}

/// T04: each tool has a name, description, and inputSchema
#[tokio::test]
async fn test_tools_have_required_fields() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = client.list_tools(None).await.expect("list_tools succeeds");

    for tool in &result.tools {
        assert!(!tool.name.is_empty(), "tool name must not be empty");
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "tool '{}' must have a non-empty description",
            tool.name
        );
        assert_eq!(
            tool.input_schema.get("type").and_then(Value::as_str),
            Some("object"),
            "tool '{}' inputSchema must be a JSON Schema object",
            tool.name
        );
    }
}

/// T17: no mutating tool is accessible (only the 4 read-only tools exist)
#[tokio::test]
async fn test_no_mutating_tools_accessible() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = client.list_tools(None).await.expect("list_tools succeeds");
    let tool_names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();

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

// ---------------------------------------------------------------------------
// Tool: list_stores
// ---------------------------------------------------------------------------

/// T07: list_stores returns all available stores
#[tokio::test]
async fn test_list_stores_returns_stores() {
    let client = client_for(make_handler_with_one_store()).await;

    let result = call_tool(&client, "list_stores", json!({}))
        .await
        .expect("list_stores succeeds");
    assert_ne!(result.is_error, Some(true), "should not be a tool error");

    let text = text_of(&result);
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
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![], embedder, false);
    let client = client_for(handler).await;

    let result = call_tool(&client, "list_stores", json!({}))
        .await
        .expect("list_stores succeeds");
    let text = text_of(&result);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["stores"].as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Tool: search
// ---------------------------------------------------------------------------

/// T09: search returns citations in the canonical JSON shape
#[tokio::test]
async fn test_search_returns_canonical_citations() {
    let (handler, _doc_id, _chunk_id) = make_handler_with_seeded_store().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "Rust programming language", "limit": 5 }),
    )
    .await
    .expect("search succeeds");
    assert_eq!(result.is_error, Some(false), "should not be a tool error");

    let text = text_of(&result);
    let json_part = text.split("\n---\n").next().unwrap_or(&text);
    let parsed: Value = serde_json::from_str(json_part).expect("valid JSON in content");

    let citations = parsed["citations"].as_array().expect("citations array");
    assert!(!citations.is_empty(), "should find at least one citation");

    let first = &citations[0];
    assert!(first.get("chunk_id").is_some(), "citation.chunk_id missing");
    assert!(
        first.get("resource_id").is_some(),
        "citation.resource_id missing"
    );
    assert!(first.get("store").is_some(), "citation.store missing");
    assert!(first.get("uri").is_some(), "citation.uri missing");
    assert!(
        first.get("title").is_some() || first.get("title").map(|v| v.is_null()).unwrap_or(true),
        "citation.title must be present (null or string)"
    );
    assert!(
        first.get("heading_path").is_some(),
        "citation.heading_path missing"
    );
    assert!(first.get("block").is_some(), "citation.block missing");
    assert!(
        first.get("chunk_position").is_some(),
        "citation.chunk_position missing"
    );
    assert!(first.get("location").is_some(), "citation.location missing");
    assert!(first.get("snippet").is_some(), "citation.snippet missing");
    assert!(first.get("score").is_some(), "citation.score missing");
    assert!(
        first.get("provenance").is_some(),
        "citation.provenance missing"
    );

    let score = &first["score"];
    assert!(score.get("fused").is_some(), "score.fused missing");
    assert!(score.get("dense").is_some(), "score.dense missing");
    assert!(score.get("bm25").is_some(), "score.bm25 missing");

    let store_obj = &first["store"];
    assert!(store_obj.get("id").is_some(), "citation.store.id missing");
    assert!(
        store_obj.get("name").is_some(),
        "citation.store.name missing"
    );

    let block = &first["block"];
    assert!(block.get("seq").is_some(), "citation.block.seq missing");
    assert!(block.get("kind").is_some(), "citation.block.kind missing");
    // #103: page from a paginated source is serialized on the MCP surface.
    assert_eq!(
        block.get("page").and_then(|p| p.as_u64()),
        Some(4),
        "citation.block.page must serialize through the MCP search surface"
    );

    assert!(
        first["chunk_position"].get("seq_in_block").is_some(),
        "citation.chunk_position.seq_in_block missing"
    );

    let span = &first["location"]["span"];
    assert!(
        span.get("start").is_some(),
        "citation.location.span.start missing"
    );
    assert!(
        span.get("end").is_some(),
        "citation.location.span.end missing"
    );

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
    let store = std::sync::Arc::new(FakeStore::new());

    let uri = "file:///docs/long.md";
    let text = "Rust programming is a systems language focused on safety. \
It prevents entire classes of memory bugs at compile time without a garbage \
collector, which keeps runtime performance predictable and fast.";
    let doc_hash = content_hash(text);
    let doc_id_val = resource_id(uri, &doc_hash);
    let span = Span::new(0, text.len());
    let cid = chunk_id(&doc_id_val, 0, text, 0);

    let record = ChunkRecord {
        id: cid,
        resource_id: doc_id_val,
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
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.to_string(),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
    };
    store.upsert_chunks(vec![record]).await.unwrap();

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![available], embedder, false);
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "Rust programming", "limit": 1, "content_length": 60 }),
    )
    .await
    .expect("search succeeds");

    let text_out = text_of(&result);

    // The JSON part must still carry the full, untruncated snippet.
    let json_part = text_out.split("\n---\n").next().unwrap_or(&text_out);
    let parsed: Value = serde_json::from_str(json_part).unwrap();
    let citations = parsed["citations"].as_array().unwrap();
    assert!(!citations.is_empty(), "should find at least one citation");
    let full_snippet = citations[0]["snippet"].as_str().unwrap();
    assert_eq!(
        full_snippet, text,
        "JSON citation snippet must remain untruncated"
    );

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
    assert!(
        snippet_line.contains("safety.…") || snippet_line.ends_with("safety…"),
        "expected snap at sentence boundary, got: {snippet_line}"
    );
}

/// T10: search with unknown store name → store_not_found tool error
#[tokio::test]
async fn test_search_unknown_store_name() {
    let (handler, _, _) = make_handler_with_seeded_store().await;
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "test", "stores": ["nonexistent-store"] }),
    )
    .await
    .expect("call succeeds at the protocol level");

    assert_eq!(result.is_error, Some(true), "should be a tool error");
    let error_text = text_of(&result);
    assert!(
        error_text.contains("store_not_found") || error_text.contains("nonexistent-store"),
        "error text should reference the missing store: {error_text}"
    );
}

/// T11 (changed expectation): search with missing query argument now fails
/// `Parameters<SearchArgs>` deserialization before `tool_search` runs — a
/// tool-level "failed to deserialize parameters" error, per rmcp 1.8.0's
/// `into_tool_argument_error` (verified empirically; see
/// `assert_deserialization_error`).
#[tokio::test]
async fn test_search_missing_query_argument() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = call_tool(&client, "search", json!({})).await;
    let text = assert_deserialization_error(result);
    assert!(
        text.contains("query"),
        "error should mention 'query': {text}"
    );
}

/// T12: search returns empty citations for a store with no content
#[tokio::test]
async fn test_search_empty_store() {
    let client = client_for(make_handler_with_one_store()).await; // store has no chunks

    let result = call_tool(&client, "search", json!({ "query": "anything" }))
        .await
        .expect("search succeeds");
    assert_eq!(result.is_error, Some(false));

    let text = text_of(&result);
    let json_part = text.split("\n---\n").next().unwrap_or(&text);
    let parsed: Value = serde_json::from_str(json_part).unwrap();
    let citations = parsed["citations"].as_array().unwrap();
    assert!(
        citations.is_empty(),
        "empty store should return no citations"
    );
}

/// T13: search limit is respected
#[tokio::test]
async fn test_search_limit_respected() {
    let store = std::sync::Arc::new(FakeStore::new());

    let mut records = Vec::new();
    for i in 0..5 {
        let text = format!("Chunk {i} about Rust programming language and systems software.");
        let uri = format!("file:///docs/doc{i}.md");
        let doc_hash = content_hash(&text);
        let doc_id_val = resource_id(&uri, &doc_hash);
        let span = Span::new(0, text.len());
        let cid = chunk_id(&doc_id_val, 0, &text, 0);

        records.push(ChunkRecord {
            id: cid,
            resource_id: doc_id_val,
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
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri,
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
        });
    }
    store.upsert_chunks(records).await.unwrap();

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(vec![available], embedder, false);
    let client = client_for(handler).await;

    let result = call_tool(
        &client,
        "search",
        json!({ "query": "Rust programming", "limit": 3 }),
    )
    .await
    .expect("search succeeds");

    let text = text_of(&result);
    let json_part = text.split("\n---\n").next().unwrap_or(&text);
    let parsed: Value = serde_json::from_str(json_part).unwrap();
    let citations = parsed["citations"].as_array().unwrap();
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

// ---------------------------------------------------------------------------
// Tool: get_chunks
// ---------------------------------------------------------------------------

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
    let text = assert_deserialization_error(result);
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

// ---------------------------------------------------------------------------
// Tool: get_chunks — anchor-relative pagination (#146)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Unknown tool
// ---------------------------------------------------------------------------

/// T18 (changed expectation): calling an unregistered tool name is now
/// dispatched by rmcp's own macro-generated `call_tool`, which returns a
/// protocol-level error rather than the old hand-written tool-level
/// `CallToolResult::error("unknown tool '...'")`. Confirmed against rmcp
/// 1.8.0 source (`handler/server/router/tool.rs`): unmatched names return
/// `ErrorData::invalid_params("tool not found", None)`.
#[tokio::test]
async fn test_unknown_tool_call() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = call_tool(&client, "add_source", json!({ "path": "/evil" })).await;

    match result {
        Err(ServiceError::McpError(e)) => {
            assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(e.message, "tool not found");
        }
        other => panic!("expected a protocol-level McpError, got {other:?}"),
    }
}
