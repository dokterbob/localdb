//! Integration tests for the `/mcp` route mounted by `build_router`
//! (Phase 2: daemon-hosted MCP over Streamable HTTP).
//!
//! `StreamableHttpService`'s tower `Service::call` returns a boxed future
//! producing a full `Response` per request/notification, so a plain
//! `tower::ServiceExt::oneshot` call against the mounted `Router` *can*
//! drive a single MCP request in-process without a real socket. But rmcp's
//! own client transport (`StreamableHttpClientTransport`, feature
//! `transport-streamable-http-client-reqwest`) speaks real HTTP over
//! `reqwest` — there is no in-process shortcut for it, and every one of
//! rmcp's own `StreamableHttpService` tests (see the crate's `tests/`
//! directory) spins up a real `tokio::net::TcpListener` + `axum::serve`
//! rather than using `oneshot`. This test does the same for a genuine
//! connect → `list_tools` → `call_tool` round trip; `oneshot` (via the
//! shared `common::request` helper) is reserved for the plain `/v1/status`
//! regression check below, which needs no session/streaming semantics at all.

mod common;

use axum::http::{Method, StatusCode};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ClientInfo},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ServiceExt,
};
use serde_json::Value;

use common::{create_store, json_body, make_app, request};

#[tokio::test]
async fn v1_status_still_answers_alongside_the_mcp_mount() {
    let (_dir, app) = make_app().await;
    create_store(app.clone(), "docs").await;

    let status = request(app, Method::GET, "/v1/status", None).await;

    assert_eq!(status.status(), StatusCode::OK);
    let body = json_body(status.into_body()).await;
    assert!(
        body.is_object(),
        "expected a JSON object body, got: {body:?}"
    );
}

#[tokio::test]
async fn mcp_route_lists_and_calls_tools_over_real_http() {
    let (_dir, app) = make_app().await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener should succeed");
    let addr = listener
        .local_addr()
        .expect("bound listener should report a local address");

    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("client should complete the MCP initialize handshake");

    let tools = client
        .list_all_tools()
        .await
        .expect("list_tools should succeed against the daemon-hosted MCP route");
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["get_chunks", "get_document", "list_stores", "search"],
        "the four read-only tools should be registered over HTTP just as over stdio"
    );

    let result = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("call_tool(list_stores) should succeed against an empty store set");
    assert_ne!(
        result.is_error,
        Some(true),
        "list_stores against zero configured stores should not be a tool-level error: {result:?}"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// The point of T2: a store created *after* the router (and thus the `/mcp`
/// service) is built must be visible over MCP without any restart. Before
/// T2, `build_router`'s `mcp_stores` argument was a `Vec<AvailableStore>`
/// snapshot taken once at construction — `McpHandler` held it directly.
/// Now `McpHandler` holds an `Arc<dyn StoreProvider>` (see
/// `mcp::store_provider`) that re-resolves the store list from the same
/// live `AppState` on every tool call, so this test creates the store via a
/// real `POST /v1/stores` call against a second `Router` handle sharing the
/// same `AppState` (through `oneshot`, alongside the `axum::serve`-driven
/// handle the MCP client actually talks to) and asserts `list_stores` picks
/// it up on the next call, no reconnect needed.
#[tokio::test]
async fn mcp_route_sees_a_store_created_after_the_router_was_built() {
    let (_dir, app) = make_app().await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener should succeed");
    let addr = listener
        .local_addr()
        .expect("bound listener should report a local address");

    // Keep a second handle to the same router/state so a store can be added
    // via a real HTTP call after the MCP service below is already up.
    let http_app = app.clone();
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, http_app).await;
    });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("client should complete the MCP initialize handshake");

    let before = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("list_stores should succeed before any store exists");
    let before_body = call_tool_json(before);
    assert_eq!(
        before_body["stores"].as_array().unwrap().len(),
        0,
        "no stores should be visible yet"
    );

    // Create the store AFTER the router/MCP service was built and the
    // client already connected — no restart, no new connection.
    create_store(app, "realtime-docs").await;

    let after = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("list_stores should succeed after the store is created");
    let after_body = call_tool_json(after);
    let names: Vec<&str> = after_body["stores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["realtime-docs"],
        "the store created after the router was built must appear over MCP without a restart"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// A `StoreProvider` failure at tool-call time must surface as a normal
/// MCP tool-level error (`CallToolResult { is_error: Some(true), .. }`), not
/// tear down the session — the next call against the same connection must
/// still be answerable.
#[tokio::test]
async fn mcp_route_reports_provider_errors_as_tool_errors_and_stays_up() {
    let dir = tempfile::tempdir().expect("tempdir for isolated test state");
    let queue = server::JobQueue::new();
    let state = server::AppState::new(
        localdb_core::config::schema::RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        },
        dir.path().to_path_buf(),
        queue.clone(),
        server::UrlRefreshScheduler::new(queue),
    )
    .await
    .expect("state should construct over a temp libsql database");

    let failing_provider: std::sync::Arc<dyn mcp::StoreProvider> =
        std::sync::Arc::new(FailingStoreProvider);
    let app = server::build_router(
        state,
        failing_provider,
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener should succeed");
    let addr = listener.local_addr().expect("listener has a local addr");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("client should complete the MCP initialize handshake");

    let result = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("a provider failure is a tool-level error, not a protocol failure");
    assert_eq!(
        result.is_error,
        Some(true),
        "provider failure should produce a tool-level error result"
    );

    // The service must still be up: a second call on the same connection
    // succeeds at the protocol level too (still a tool error, since the
    // provider always fails, but the session itself is unharmed).
    let second = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("the session must survive a provider error and answer a second call");
    assert_eq!(second.is_error, Some(true));

    let _ = client.cancel().await;
    server_task.abort();
}

/// A `StoreProvider` that always fails, for
/// `mcp_route_reports_provider_errors_as_tool_errors_and_stays_up`.
struct FailingStoreProvider;

#[async_trait::async_trait]
impl mcp::StoreProvider for FailingStoreProvider {
    async fn available_stores(&self) -> Result<Vec<mcp::AvailableStore>, localdb_core::Error> {
        Err(localdb_core::Error::Internal {
            message: "simulated backend outage".to_string(),
            correlation_id: "test_failing_store_provider".to_string(),
        })
    }
}

/// Parse a `CallToolResult`'s first text content item as JSON.
fn call_tool_json(result: CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("expected a text content item");
    serde_json::from_str(&text).expect("content should be valid JSON")
}
