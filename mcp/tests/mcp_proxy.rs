//! Proxy-transparency test for `mcp::proxy::ProxyHandler` (Phase 3 scope).
//!
//! Spins up a real Streamable HTTP MCP service on a genuine TCP listener —
//! the same shape `server/tests/mcp_route.rs` proved works for Phase 2 — to
//! stand in for a running daemon's `/mcp` route. `ProxyHandler` connects to
//! it exactly as `cli::cmds::surface::run_mcp_async` would. Then
//! `ProxyHandler` itself is served over an in-memory duplex pair — the same
//! shape a real stdio caller sees, per `mcp/tests/mcp_protocol.rs` — and
//! driven with a plain client, asserting `list_tools`/`call_tool` pass
//! through unchanged end to end: stdio caller -> `ProxyHandler` -> real HTTP
//! -> upstream `McpHandler`.

use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;

use rmcp::{
    model::CallToolRequestParams,
    service::{RoleClient, RunningService},
    ServiceExt,
};

use localdb_core::{
    ids::{chunk_id, content_hash, new_ulid, resource_id},
    store::{ChunkRecord, FakeStore, RetrievalStore},
    types::Span,
    FakeEmbedder,
};
use mcp::{proxy::ProxyHandler, AvailableStore, StoreDescriptor};

/// Start a real upstream MCP-over-HTTP "daemon" seeded with one store
/// holding one chunk. Returns its bare base URL (no `/mcp` suffix — matches
/// `probe_daemon`'s `DaemonState::Running::base_url` shape, which
/// `ProxyHandler::connect` appends `/mcp` to itself) plus the seeded
/// document's id, for a `get_chunks` round trip.
async fn start_upstream_daemon() -> (String, String) {
    let store = Arc::new(FakeStore::new());

    let uri = "file:///docs/proxy-test.md";
    let doc_hash = content_hash("proxy transparency test content");
    let doc_id = resource_id(uri, &doc_hash);
    let snippet = "The proxy must forward this citation unchanged end to end.";
    let span = Span::new(0, snippet.len());
    let cid = chunk_id(&doc_id, 0, snippet, 0);

    let record = ChunkRecord {
        id: cid,
        resource_id: doc_id.clone(),
        store_id: "store-1".to_string(),
        text: snippet.to_string(),
        span,
        heading_path: vec![],
        embedding: vec![0.8, 0.2, 0.1, 0.5],
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
    store.upsert_chunks(vec![record]).await.expect("seed chunk");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "proxy-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: Arc<dyn localdb_core::Embedder> = Arc::new(FakeEmbedder::new(4));

    // `vec![]` disables rmcp's Host-header allowlist entirely — this test
    // exercises proxy forwarding, not the allowlist itself, and connects
    // over a real loopback socket regardless.
    let service = mcp::build_streamable_http_service(vec![available], embedder, vec![]);
    let app = Router::new().nest_service("/mcp", service);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener");
    let addr = listener.local_addr().expect("listener has a local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), doc_id)
}

/// Serve `handler` on one half of an in-memory duplex pipe and connect a
/// trivial (no-op) client to the other half — the same harness
/// `mcp/tests/mcp_protocol.rs` uses to drive `McpHandler`, reused here for
/// `ProxyHandler`'s stdio-facing side (only the *other* hop, proxy ->
/// upstream, needs a genuine socket).
async fn client_for(handler: ProxyHandler) -> RunningService<RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        match handler.serve(server_transport).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => panic!("proxy failed to initialize: {e}"),
        }
    });
    ().serve(client_transport)
        .await
        .expect("client should connect to the proxy")
}

#[tokio::test]
async fn proxy_forwards_tool_list_and_calls_unchanged() {
    let (daemon_base_url, doc_id) = start_upstream_daemon().await;

    let proxy = ProxyHandler::connect(&daemon_base_url)
        .await
        .expect("proxy should connect to the upstream daemon");
    let client = client_for(proxy).await;

    let tools = client.list_tools(None).await.expect("list_tools succeeds");
    let mut names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["get_chunks", "get_document", "list_stores", "search"],
        "the proxy must expose exactly the upstream's tool set, unchanged"
    );

    let list_stores_result = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("call_tool(list_stores) should succeed through the proxy");
    assert_ne!(list_stores_result.is_error, Some(true));
    let text = list_stores_result.content[0]
        .as_text()
        .unwrap()
        .text
        .clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    let stores = parsed["stores"].as_array().expect("stores array");
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0]["name"], "proxy-store");

    let get_chunks_args = serde_json::json!({ "resource_id": doc_id })
        .as_object()
        .cloned()
        .unwrap();
    let get_chunks_result = client
        .call_tool(CallToolRequestParams::new("get_chunks").with_arguments(get_chunks_args))
        .await
        .expect("call_tool(get_chunks) should succeed through the proxy");
    assert_ne!(get_chunks_result.is_error, Some(true));
    let text = get_chunks_result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(parsed["resource_id"], doc_id);
    assert_eq!(parsed["total_chunks"], 1);

    let _ = client.cancel().await;
}

#[tokio::test]
async fn proxy_forwards_protocol_level_error_for_unknown_tool_unchanged() {
    let (daemon_base_url, _doc_id) = start_upstream_daemon().await;

    let proxy = ProxyHandler::connect(&daemon_base_url)
        .await
        .expect("proxy should connect to the upstream daemon");
    let client = client_for(proxy).await;

    let result = client
        .call_tool(CallToolRequestParams::new("nonexistent_tool"))
        .await;

    match result {
        Err(rmcp::ServiceError::McpError(e)) => {
            // Same message rmcp's own macro-generated dispatch produces for
            // an unregistered tool name (see `mcp/tests/mcp_protocol.rs`'s
            // `test_unknown_tool_call`) — proves the proxy forwarded the
            // upstream's protocol-level tier rather than downgrading it to
            // a tool-level error of its own.
            assert_eq!(e.message, "tool not found");
        }
        other => {
            panic!("expected a protocol-level McpError forwarded from upstream, got {other:?}")
        }
    }

    let _ = client.cancel().await;
}
