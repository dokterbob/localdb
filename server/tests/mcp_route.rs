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
    model::{CallToolRequestParams, ClientInfo},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ServiceExt,
};

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
        vec![
            "get_chunks",
            "get_document",
            "list_documents",
            "list_stores",
            "search"
        ],
        "the five read-only tools should be registered over HTTP just as over stdio"
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
