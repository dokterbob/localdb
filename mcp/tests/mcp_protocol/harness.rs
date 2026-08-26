//! Shared test harness: an in-memory duplex transport plus fixture builders
//! reused across every `mcp_protocol` test module.

use serde_json::Value;

use rmcp::{
    model::{CallToolRequestParams, CallToolResult},
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

/// Serve `handler` on one half of an in-memory duplex pipe and connect a
/// trivial (no-op) client to the other half — the same shape a real stdio
/// MCP client/server pair has, without an OS pipe.
pub(crate) async fn client_for(handler: McpHandler) -> RunningService<RoleClient, ()> {
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
pub(crate) async fn call_tool(
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
pub(crate) fn text_of(result: &CallToolResult) -> String {
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
pub(crate) fn assert_deserialization_error(result: Result<CallToolResult, ServiceError>) -> String {
    let result = result.expect("deserialization failures are tool-level, not protocol-level");
    assert_eq!(result.is_error, Some(true), "should be a tool error");
    let text = text_of(&result);
    assert!(
        text.starts_with("failed to deserialize parameters:"),
        "expected a parameter-deserialization error, got: {text}"
    );
    text
}

/// Build an `McpHandler` over `stores`, deriving its `StoreBackend` from the
/// same `AvailableStore` fixtures (via `mcp::tools::StoresBackend`) rather
/// than standing up a second, parallel document registry per test — see
/// `StoresBackend`'s own doc comment.
pub(crate) fn handler_with_stores(
    stores: Vec<AvailableStore>,
    embedder: std::sync::Arc<dyn localdb_core::Embedder>,
    allow_write: bool,
) -> McpHandler {
    let backend: std::sync::Arc<dyn localdb_core::StoreBackend> =
        std::sync::Arc::new(mcp::tools::StoresBackend::new(&stores));
    McpHandler::new(stores, backend, embedder, allow_write)
}

/// Build a handler with one empty store.
pub(crate) fn make_handler_with_one_store() -> McpHandler {
    let store = std::sync::Arc::new(FakeStore::new());
    let sd = StoreDescriptor {
        id: new_ulid(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    handler_with_stores(vec![available], embedder, false)
}

/// Build a handler with one store seeded with a chunk.
pub(crate) async fn make_handler_with_seeded_store() -> (McpHandler, String, String) {
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
        modified_at: Some("2026-06-10T12:00:00Z".to_string()),
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
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
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
    let handler = handler_with_stores(vec![available], embedder, false);
    (handler, doc_id, cid)
}

/// Build a handler seeded with ONE document made of 3 chunks, inserted out
/// of storage order. Proves that `get_chunks` sorts defensively by
/// `(block_seq, seq_in_block)` rather than trusting insertion/store order
/// (unlike libsql, `FakeStore` does not guarantee ordering).
pub(crate) async fn make_handler_with_multichunk_doc() -> (McpHandler, String) {
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
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
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
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
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
    let handler = handler_with_stores(vec![available], embedder, false);
    (handler, doc_id)
}

/// Build a handler seeded with ONE document whose two chunks both have
/// `(block_seq, seq_in_block) = (0, 0)` and an identical span, so the ONLY
/// distinguishing sort field is `chunk_id`. The two records are inserted in
/// an order controlled by `reversed` — because `FakeStore` preserves
/// insertion order, this exercises whether `get_chunks` imposes a stable
/// total order (by `chunk_id`) regardless of backend return order.
pub(crate) async fn make_handler_with_tied_chunks(reversed: bool) -> (McpHandler, String) {
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
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
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
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
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
    let handler = handler_with_stores(vec![available], embedder, false);
    (handler, doc_id)
}

/// Build a handler seeded with ONE document made of `count` chunks, one per
/// block (`block_seq` 0..count, `seq_in_block` 0) — mirrors the shape of the
/// spec's worked anchor-pagination example (specs/05-surfaces.md §4.1: 20
/// chunks, one chunk per block). Returns the handler, the resource id, and
/// the chunk ids in `(block_seq, seq_in_block)` order (index == block_seq).
pub(crate) async fn make_handler_with_sequential_chunks(
    count: u32,
) -> (McpHandler, String, Vec<String>) {
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
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
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
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
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
    let handler = handler_with_stores(vec![available], embedder, false);
    (handler, doc_id, ids)
}

/// Build a handler seeded with ONE document with a gap in `block_seq` and a
/// block holding multiple chunks, for `anchor_block_seq` lower-bound and
/// tie-break tests (#146): `block_seq` 0 (one chunk), `block_seq` 5 (three
/// chunks, `seq_in_block` 0/1/2, inserted out of order), `block_seq` 10 (one
/// chunk).
pub(crate) async fn make_handler_with_block_seq_gaps() -> (McpHandler, String) {
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
            modified_at: Some("2026-06-10T12:00:00Z".to_string()),
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
            date_original: None,
            date_parsed: None,
            external_id: None,
            external_etag: None,
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
    let handler = handler_with_stores(vec![available], embedder, false);
    (handler, doc_id)
}
