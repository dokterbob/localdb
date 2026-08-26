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
    metadata::Metadata,
    store::{ChunkRecord, FakeStore, RetrievalStore},
    types::Span,
    DocumentInfo, Error, FakeEmbedder, SourceRow, StoreBackend, StoreBackendConfig, StoreRow,
    TableSize,
};
use mcp::{proxy::ProxyHandler, AvailableStore, StoreDescriptor};

// ---------------------------------------------------------------------------
// A `StoreBackend` whose only real behavior is `list_documents`/
// `count_documents` — `mcp::tools::StoresBackend` (used by every other proxy
// test in this file) deliberately leaves those `unimplemented!()`, since it
// only exists to back `get_document`'s two calls
// (`find_document`/`retrieval_store`). Modeled on `core/src/documents/
// tests.rs`'s own `FakeBackend`.
// ---------------------------------------------------------------------------

struct DocumentRegistryBackend {
    documents: std::collections::HashMap<String, Vec<DocumentInfo>>,
}

#[async_trait::async_trait]
impl StoreBackend for DocumentRegistryBackend {
    async fn open(_config: StoreBackendConfig) -> Result<Self, Error> {
        unimplemented!("never constructed via the trait's own open()")
    }
    async fn upsert_store(&self, _store: &StoreRow) -> Result<(), Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn delete_store(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn get_store(&self, _id: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn get_store_by_name(&self, _name: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn upsert_source(&self, _source: &SourceRow) -> Result<(), Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn delete_source(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn get_source(&self, _id: &str) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn list_sources(&self, _store_id: &str) -> Result<Vec<SourceRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn find_source_by_root_or_url(
        &self,
        _value: &str,
        _store_id: &str,
    ) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn find_document(
        &self,
        _doc_id: &str,
        _store_id: Option<&str>,
    ) -> Result<Option<DocumentInfo>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn list_documents(
        &self,
        store_id: &str,
        _source_id: Option<&str>,
        _limit: Option<usize>,
        _offset: usize,
    ) -> Result<Vec<DocumentInfo>, Error> {
        Ok(self.documents.get(store_id).cloned().unwrap_or_default())
    }
    async fn count_documents(
        &self,
        store_id: &str,
        _source_id: Option<&str>,
    ) -> Result<u64, Error> {
        Ok(self.documents.get(store_id).map(|d| d.len()).unwrap_or(0) as u64)
    }
    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        Err(Error::StoreNotFound {
            id: store_id.to_string(),
        })
    }
    async fn largest_tables(&self, _limit: usize) -> Result<Vec<TableSize>, Error> {
        Ok(Vec::new())
    }
}

fn make_document_info(id: &str, store_id: &str, uri: &str) -> DocumentInfo {
    DocumentInfo {
        store_id: store_id.to_string(),
        id: id.to_string(),
        source_id: "src-1".to_string(),
        ingestor_kind: "path".to_string(),
        uri: uri.to_string(),
        title: None,
        mime: None,
        content_hash: "hash".to_string(),
        fetched_at: "2026-01-01T00:00:00Z".to_string(),
        origin_store: store_id.to_string(),
        policy_version: "v1".to_string(),
        metadata: Metadata::default(),
    }
}

/// Build one `AvailableStore` seeded with a single chunk, returning it
/// alongside the seeded document's id (for `get_chunks`/`get_document` round
/// trips).
async fn seeded_store(store_id: &str, store_name: &str, text: &str) -> (AvailableStore, String) {
    let store = Arc::new(FakeStore::new());

    let uri = format!("file:///docs/{store_name}.md");
    let doc_hash = content_hash(text);
    let doc_id = resource_id(&uri, &doc_hash);
    let span = Span::new(0, text.len());
    let cid = chunk_id(&doc_id, 0, text, 0);

    let record = ChunkRecord {
        id: cid,
        resource_id: doc_id.clone(),
        store_id: store_id.to_string(),
        text: text.to_string(),
        span,
        heading_path: vec![],
        embedding: vec![0.8, 0.2, 0.1, 0.5],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        modified_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: doc_hash,
        origin_store: store_id.to_string(),
        source_id: new_ulid(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: uri.clone(),
        metadata: localdb_core::metadata::Metadata::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
    };
    store.upsert_chunks(vec![record]).await.expect("seed chunk");

    let sd = StoreDescriptor {
        id: store_id.to_string(),
        name: store_name.to_string(),
        visibility: "private".to_string(),
    };
    (AvailableStore::from_arc(sd, store), doc_id)
}

/// Serve the given stores as a real upstream MCP-over-HTTP "daemon", backed
/// by `mcp::tools::StoresBackend` — the shared test double every proxy test
/// but the `list_documents`-success one uses. `StoresBackend` only backs
/// `get_document`'s two calls (`find_document`/`retrieval_store`);
/// `list_documents`/`count_documents` are `unimplemented!()` on it, so a
/// test that needs a real `list_documents` answer from upstream must use
/// `serve_upstream_with_backend` with a backend of its own instead.
///
/// Returns its bare base URL (no `/mcp` suffix — matches `probe_daemon`'s
/// `DaemonState::Running::base_url` shape, which `ProxyHandler::connect`
/// appends `/mcp` to itself).
async fn serve_upstream(stores: Vec<AvailableStore>) -> String {
    let backend: Arc<dyn localdb_core::StoreBackend> =
        Arc::new(mcp::tools::StoresBackend::new(&stores));
    serve_upstream_with_backend(stores, backend).await
}

/// Serve the given stores as a real upstream MCP-over-HTTP "daemon" over an
/// explicit `StoreBackend`, for tests that need a tool `StoresBackend`
/// doesn't fully back (e.g. `list_documents`).
async fn serve_upstream_with_backend(
    stores: Vec<AvailableStore>,
    backend: Arc<dyn localdb_core::StoreBackend>,
) -> String {
    let embedder: Arc<dyn localdb_core::Embedder> = Arc::new(FakeEmbedder::new(4));

    // `vec![]` disables rmcp's Host-header allowlist entirely — these tests
    // exercise proxy forwarding, not the allowlist itself, and connect
    // over a real loopback socket regardless.
    let service = mcp::build_streamable_http_service(stores, backend, embedder, vec![]);
    let app = Router::new().nest_service("/mcp", service);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback listener");
    let addr = listener.local_addr().expect("listener has a local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    format!("http://{addr}")
}

/// Start a real upstream "daemon" seeded with one store holding one chunk.
/// Returns its base URL plus the seeded document's id.
async fn start_upstream_daemon() -> (String, String) {
    let (available, doc_id) = seeded_store(
        "store-1",
        "proxy-store",
        "The proxy must forward this citation unchanged end to end.",
    )
    .await;
    (serve_upstream(vec![available]).await, doc_id)
}

/// Start a two-store upstream — `books` and `hydra`, the shape issue #201's
/// reporter described — for the `--store` scoping tests. Returns the base URL
/// and each store's seeded document id.
async fn start_two_store_daemon() -> (String, String, String) {
    let (books, books_doc) =
        seeded_store("id-books", "books", "A passage from the books store.").await;
    let (hydra, hydra_doc) =
        seeded_store("id-hydra", "hydra", "A passage from the hydra store.").await;
    (
        serve_upstream(vec![books, hydra]).await,
        books_doc,
        hydra_doc,
    )
}

/// Start a two-store upstream (`books`/`hydra`, same ids/names as
/// `start_two_store_daemon`) backed by `DocumentRegistryBackend`, so
/// `list_documents` returns real data instead of panicking on
/// `StoresBackend`'s `unimplemented!()`. Each store holds one document.
async fn start_two_store_daemon_with_documents() -> String {
    let books = AvailableStore::new(
        StoreDescriptor {
            id: "id-books".to_string(),
            name: "books".to_string(),
            visibility: "private".to_string(),
        },
        Box::new(FakeStore::new()),
    );
    let hydra = AvailableStore::new(
        StoreDescriptor {
            id: "id-hydra".to_string(),
            name: "hydra".to_string(),
            visibility: "private".to_string(),
        },
        Box::new(FakeStore::new()),
    );

    let mut documents = std::collections::HashMap::new();
    documents.insert(
        "id-books".to_string(),
        vec![make_document_info(
            "doc-books-1",
            "id-books",
            "file:///books/1.md",
        )],
    );
    documents.insert(
        "id-hydra".to_string(),
        vec![make_document_info(
            "doc-hydra-1",
            "id-hydra",
            "file:///hydra/1.md",
        )],
    );
    let backend: Arc<dyn StoreBackend> = Arc::new(DocumentRegistryBackend { documents });

    serve_upstream_with_backend(vec![books, hydra], backend).await
}

/// Connect a proxy scoped to `books` against a two-store upstream.
async fn scoped_to_books(base_url: &str) -> ProxyHandler {
    ProxyHandler::connect(base_url, &["books".to_string()])
        .await
        .expect("a scope naming an existing store should connect")
}

/// Parse the JSON payload out of a tool result's single text content item.
///
/// Most tools return pure JSON (`tools::success_json`), but `search` returns
/// `{json}\n\n---\n{human text rendering}` in one item — so split on that
/// separator before parsing rather than assuming the whole item is JSON.
fn result_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result.content[0].as_text().unwrap().text.clone();
    let json_part = text.split("\n\n---\n").next().unwrap_or(&text);
    serde_json::from_str(json_part)
        .unwrap_or_else(|e| panic!("tool result should carry JSON text content ({e}): {text}"))
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

    let proxy = ProxyHandler::connect(&daemon_base_url, &[])
        .await
        .expect("proxy should connect to the upstream daemon");
    let client = client_for(proxy).await;

    let tools = client.list_tools(None).await.expect("list_tools succeeds");
    let mut names: Vec<&str> = tools.tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "get_chunks",
            "get_document",
            "list_documents",
            "list_stores",
            "search"
        ],
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

    let proxy = ProxyHandler::connect(&daemon_base_url, &[])
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

// ---------------------------------------------------------------------------
// `--store` scoping in proxied mode (issue #201, specs/05-surfaces.md §4.2.1)
//
// The daemon's `/mcp` route has no per-connection store filter to offer, so
// the proxy enforces the scope on tool *arguments*. These tests drive the
// full stack — stdio caller -> scoped `ProxyHandler` -> real HTTP -> upstream
// `McpHandler` over two stores — and assert on what the caller can actually
// reach, not on the injection mechanics.
// ---------------------------------------------------------------------------

/// An agent scoped to `books` must not be able to *enumerate* `hydra`.
/// Filtering `list_stores` matters as much as filtering reads: a name it can
/// see is a name it can try passing to `search`.
#[tokio::test]
async fn proxy_scoped_list_stores_hides_out_of_scope_stores() {
    let (base_url, _books_doc, _hydra_doc) = start_two_store_daemon().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    let result = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("list_stores succeeds through a scoped proxy");
    assert_ne!(result.is_error, Some(true));

    let stores = result_json(&result)["stores"]
        .as_array()
        .expect("stores array")
        .clone();
    assert_eq!(
        stores.len(),
        1,
        "only the scoped store may be listed: {stores:?}"
    );
    assert_eq!(stores[0]["name"], "books");

    let _ = client.cancel().await;
}

/// `search` with no `stores` argument must be narrowed to the scope, not left
/// to mean "every store the daemon has" — this is the case an agent hits by
/// default, so getting it wrong would make the whole flag decorative.
#[tokio::test]
async fn proxy_scoped_search_injects_scope_when_stores_absent() {
    let (base_url, _books_doc, _hydra_doc) = start_two_store_daemon().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    let args = serde_json::json!({ "query": "passage" })
        .as_object()
        .cloned()
        .unwrap();
    let result = client
        .call_tool(CallToolRequestParams::new("search").with_arguments(args))
        .await
        .expect("search succeeds through a scoped proxy");
    assert_ne!(result.is_error, Some(true));

    let citations = result_json(&result)["citations"]
        .as_array()
        .expect("citations array")
        .clone();
    assert!(
        !citations.is_empty(),
        "the scoped store's own content must still be reachable"
    );
    for c in &citations {
        assert_eq!(
            c["store"]["name"], "books",
            "a bare search must not return hits from outside the scope: {c}"
        );
    }

    let _ = client.cancel().await;
}

/// Naming an out-of-scope store explicitly is a tool-level `invalid_request`
/// that names the allowed set, rather than silently returning that store's
/// hits.
#[tokio::test]
async fn proxy_scoped_search_rejects_out_of_scope_store_name() {
    let (base_url, _books_doc, _hydra_doc) = start_two_store_daemon().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    let args = serde_json::json!({ "query": "passage", "stores": ["hydra"] })
        .as_object()
        .cloned()
        .unwrap();
    let result = client
        .call_tool(CallToolRequestParams::new("search").with_arguments(args))
        .await
        .expect("the rejection is a tool result, not a transport failure");

    assert_eq!(result.is_error, Some(true));
    let parsed = result_json(&result);
    assert_eq!(parsed["error"]["code"], "invalid_request");
    let message = parsed["error"]["message"].as_str().unwrap();
    assert!(message.contains("hydra"), "{message}");
    assert!(
        message.contains("books"),
        "the error should name the allowed set: {message}"
    );

    let _ = client.cancel().await;
}

/// The leak this prevents: `get_document`'s `store` argument accepts any
/// store the daemon has. Injecting the scope only when the argument is
/// *absent* would leave an explicit out-of-scope value working perfectly,
/// which is exactly how an agent would read another project's docs.
#[tokio::test]
async fn proxy_scoped_get_document_rejects_out_of_scope_explicit_store() {
    let (base_url, _books_doc, hydra_doc) = start_two_store_daemon().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    // Both by name and by id — #144 lets a caller round-trip either from a
    // citation, so both have to be closed.
    for store_value in ["hydra", "id-hydra"] {
        let args = serde_json::json!({ "id": hydra_doc, "store": store_value })
            .as_object()
            .cloned()
            .unwrap();
        let result = client
            .call_tool(CallToolRequestParams::new("get_document").with_arguments(args))
            .await
            .expect("the rejection is a tool result, not a transport failure");

        assert_eq!(
            result.is_error,
            Some(true),
            "store='{store_value}' is outside the scope and must be refused"
        );
        let parsed = result_json(&result);
        assert_eq!(parsed["error"]["code"], "invalid_request");
        assert!(
            !parsed.to_string().contains("hydra store"),
            "the refusal must not carry the out-of-scope document's content: {parsed}"
        );
    }

    let _ = client.cancel().await;
}

/// `list_documents`' `store` argument is required (unlike `get_document`'s/
/// `get_chunks`' optional one), so a scoped proxy must never inject a store
/// on the caller's behalf: an omitted `store` has to surface the upstream's
/// own missing-required-argument error, not silently return whichever
/// scoped store happens to come first.
#[tokio::test]
async fn proxy_scoped_list_documents_omitted_store_errors_instead_of_picking_first_store() {
    let (base_url, _books_doc, _hydra_doc) = start_two_store_daemon().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    let result = client
        .call_tool(CallToolRequestParams::new("list_documents"))
        .await
        .expect("a missing required argument is a tool-level error, not a transport failure");

    assert_eq!(
        result.is_error,
        Some(true),
        "omitted store must be an error"
    );
    let text = result.content[0].as_text().unwrap().text.clone();
    assert!(
        text.starts_with("failed to deserialize parameters:"),
        "an omitted `store` must surface the same missing-required-argument error embedded \
         mode and an unscoped proxy would give, not a `books`-store result: {text}"
    );

    let _ = client.cancel().await;
}

/// An explicit, in-scope `store` still works normally through a scoped
/// proxy — the fix for the omitted-store case must not turn `list_documents`
/// into a tool that can never succeed when scoped.
#[tokio::test]
async fn proxy_scoped_list_documents_explicit_in_scope_store_works() {
    let base_url = start_two_store_daemon_with_documents().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    let args = serde_json::json!({ "store": "books" })
        .as_object()
        .cloned()
        .unwrap();
    let result = client
        .call_tool(CallToolRequestParams::new("list_documents").with_arguments(args))
        .await
        .expect("list_documents succeeds through a scoped proxy for an in-scope store");

    assert_ne!(
        result.is_error,
        Some(true),
        "an in-scope explicit store must not be rejected: {:?}",
        result_json(&result)
    );
    let parsed = result_json(&result);
    assert_eq!(parsed["store"]["name"], "books");
    assert_eq!(parsed["total"], 1);

    let _ = client.cancel().await;
}

/// Naming an out-of-scope store explicitly on `list_documents` is a
/// tool-level `invalid_request` naming the allowed set, same as
/// `get_document`/`get_chunks`/`search` — never the store's own results.
#[tokio::test]
async fn proxy_scoped_list_documents_rejects_out_of_scope_store() {
    let (base_url, _books_doc, _hydra_doc) = start_two_store_daemon().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    let args = serde_json::json!({ "store": "hydra" })
        .as_object()
        .cloned()
        .unwrap();
    let result = client
        .call_tool(CallToolRequestParams::new("list_documents").with_arguments(args))
        .await
        .expect("the rejection is a tool result, not a transport failure");

    assert_eq!(result.is_error, Some(true));
    let parsed = result_json(&result);
    assert_eq!(parsed["error"]["code"], "invalid_request");
    let message = parsed["error"]["message"].as_str().unwrap();
    assert!(message.contains("hydra"), "{message}");
    assert!(
        message.contains("books"),
        "the error should name the allowed set: {message}"
    );

    let _ = client.cancel().await;
}

/// Without `--store`, nothing above applies: the proxy stays the verbatim
/// relay it has always been, including reaching every upstream store. This is
/// the control for all the scoped assertions.
#[tokio::test]
async fn proxy_unscoped_relays_verbatim() {
    let (base_url, _books_doc, hydra_doc) = start_two_store_daemon().await;
    let proxy = ProxyHandler::connect(&base_url, &[])
        .await
        .expect("an unscoped proxy connects");
    let client = client_for(proxy).await;

    // Every store is enumerable.
    let listed = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("list_stores succeeds");
    let names: std::collections::HashSet<String> = result_json(&listed)["stores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["books", "hydra"].iter().map(|s| s.to_string()).collect(),
        "an unscoped proxy must expose the daemon's full store set"
    );

    // And an explicit store argument passes straight through.
    let args = serde_json::json!({ "id": hydra_doc, "store": "hydra" })
        .as_object()
        .cloned()
        .unwrap();
    let result = client
        .call_tool(CallToolRequestParams::new("get_document").with_arguments(args))
        .await
        .expect("get_document succeeds");
    assert_ne!(
        result.is_error,
        Some(true),
        "an unscoped proxy must not reject any store: {:?}",
        result_json(&result)
    );

    let _ = client.cancel().await;
}

/// A `--store` name the daemon doesn't have fails at connect time with a
/// distinguishable error, so `cli` can map it to `store_not_found`/exit 3 —
/// the same answer embedded mode gives. Collapsing it into the generic
/// connect failure would report "daemon is unreachable" for a daemon that
/// answered perfectly well.
#[tokio::test]
async fn proxy_connect_unknown_store_name_errors() {
    let (base_url, _books_doc, _hydra_doc) = start_two_store_daemon().await;

    let err = ProxyHandler::connect(&base_url, &["nosuchstore".to_string()])
        .await
        .err()
        .expect("an unknown --store name must fail the connect");

    match err {
        mcp::ProxyConnectError::StoreNotFound(name) => assert_eq!(name, "nosuchstore"),
        other => panic!("expected StoreNotFound (-> exit 3), got {other:?}"),
    }
}

/// A scoped session serves only the five tools whose store semantics the
/// proxy knows. Any other name is refused rather than relayed, so the first
/// mutating tool added under `--allow-write` cannot silently bypass the
/// scope on the day it lands.
#[tokio::test]
async fn proxy_scoped_refuses_tools_without_a_scoping_rule() {
    let (base_url, _books_doc, _hydra_doc) = start_two_store_daemon().await;
    let client = client_for(scoped_to_books(&base_url).await).await;

    let result = client
        .call_tool(CallToolRequestParams::new("some_future_write_tool"))
        .await
        .expect("the refusal is a tool result authored by the proxy");
    assert_eq!(result.is_error, Some(true));
    let parsed = result_json(&result);
    assert_eq!(parsed["error"]["code"], "invalid_request");
    assert!(parsed["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no --store scoping rule"));

    let _ = client.cancel().await;
}

/// The tripwire for `--allow-write` (specs/05-surfaces.md §4): v1 registers
/// no mutating tool, so the tool set must be identical with and without the
/// flag — which is why `localdb mcp --allow-write` only warns.
///
/// When a mutating tool *is* added, this test fails, and whoever adds it must
/// revisit both that warning and the proxy's tool gate above.
#[tokio::test]
async fn mcp_tool_set_identical_with_and_without_allow_write() {
    /// Serve an `McpHandler` over a duplex pair (the harness
    /// `mcp/tests/mcp_protocol.rs` uses) and ask it for its tool set — going
    /// through the real `tools/list` request rather than reading the
    /// router field, so this asserts on what a *client* would actually see.
    async fn served_tool_names(allow_write: bool) -> Vec<String> {
        let store = AvailableStore::new(
            StoreDescriptor {
                id: "s1".to_string(),
                name: "s1".to_string(),
                visibility: "private".to_string(),
            },
            Box::new(FakeStore::new()),
        );
        let stores = vec![store];
        let backend: Arc<dyn localdb_core::StoreBackend> =
            Arc::new(mcp::tools::StoresBackend::new(&stores));
        let embedder: Arc<dyn localdb_core::Embedder> = Arc::new(FakeEmbedder::new(4));
        let handler = mcp::McpHandler::new(stores, backend, embedder, allow_write);

        let (server_transport, client_transport) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            if let Ok(running) = handler.serve(server_transport).await {
                let _ = running.waiting().await;
            }
        });
        let client: RunningService<RoleClient, ()> =
            ().serve(client_transport)
                .await
                .expect("client should connect");

        let tools = client.list_tools(None).await.expect("list_tools succeeds");
        let mut names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        let _ = client.cancel().await;
        names
    }

    let without = served_tool_names(false).await;
    let with = served_tool_names(true).await;

    assert_eq!(
        without,
        vec![
            "get_chunks",
            "get_document",
            "list_documents",
            "list_stores",
            "search"
        ],
        "v1's read-only tool set"
    );
    assert_eq!(
        with, without,
        "`--allow-write` registers no additional tool in v1 — if this fails, a mutating \
         tool was added: revisit the CLI's no-op warning AND `ProxyHandler::call_tool`'s \
         scoping gate, which currently refuses every tool outside this five-tool set"
    );
}
