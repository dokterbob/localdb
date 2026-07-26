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
        server::AuthMode::Open,
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

/// R4 (the critical propagation pin): with auth **enforced**, the axum
/// `require_auth` middleware authenticates the bearer token and inserts the
/// resulting `Principal` into the request extensions; rmcp's Streamable
/// HTTP transport then injects the remaining `http::request::Parts` into
/// the tool call's `RequestContext.extensions`, where `McpHandler` reads
/// the `Principal` back out (`Parts.extensions`). Under enforced mode the
/// handler is constructed with `default_principal = None`, so this call
/// succeeding end-to-end proves the extension genuinely propagated — if it
/// hadn't, the handler would fail closed with an `unauthorized` tool error,
/// and if the middleware hadn't run at all the request would 401 before
/// rmcp ever saw it.
#[tokio::test]
async fn enforced_mcp_propagates_principal_from_middleware_into_tool_handler() {
    let (_dir, state, app) = common::make_enforced_app().await;

    // Seed an admin + API key through the same persistent AuthService the
    // router's middleware consults.
    let user = state
        .auth()
        .create_user("mcp-admin", localdb_core::auth::Role::Admin)
        .await
        .expect("seed admin");
    let secret = state
        .auth()
        .issue_api_key(&user.id)
        .await
        .expect("mint api key")
        .secret;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener should succeed");
    let addr = listener.local_addr().expect("listener has a local addr");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // rmcp's client transport sends `Authorization: Bearer <auth_header>`
    // on every request (initialize, tool calls, SSE stream alike).
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
            .auth_header(secret),
    );
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("authenticated client should complete the MCP initialize handshake");

    let result = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("authenticated call_tool should succeed at the protocol level");
    assert_ne!(
        result.is_error,
        Some(true),
        "admin principal must reach the tool handler through rmcp's extension \
         propagation — a tool-level error here means the Principal was lost \
         (fail-closed path fired): {result:?}"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// The counterpart: without a bearer token the middleware rejects the
/// request before rmcp sees it, so the MCP initialize handshake itself
/// fails — an unauthenticated client cannot even open a session.
#[tokio::test]
async fn enforced_mcp_rejects_unauthenticated_client_at_handshake() {
    let (_dir, _state, app) = common::make_enforced_app().await;

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
    let handshake = ClientInfo::default().serve(transport).await;
    assert!(
        handshake.is_err(),
        "initialize without a bearer token must fail against an enforcing daemon"
    );

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

// ---------------------------------------------------------------------------
// T5: D7 store-grant filtering over MCP (D12 composition — the realtime
// StoreProvider pin means a grant change is visible on the very next tool
// call, no reconnect or restart needed).
// ---------------------------------------------------------------------------

/// Spin up a real HTTP server for `app` and connect an MCP client to it,
/// optionally with a bearer `auth_header`. Returns the connected client and
/// the server task handle (abort it when done, matching every other test in
/// this file).
async fn connect_mcp_client(
    app: axum::Router,
    bearer: Option<&str>,
) -> (
    rmcp::service::RunningService<rmcp::RoleClient, ClientInfo>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binding a loopback listener should succeed");
    let addr = listener.local_addr().expect("listener has a local addr");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut config = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"));
    if let Some(secret) = bearer {
        config = config.auth_header(secret.to_string());
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    let client = ClientInfo::default()
        .serve(transport)
        .await
        .expect("client should complete the MCP initialize handshake");
    (client, server_task)
}

fn list_stores_names(result: CallToolResult) -> Vec<String> {
    let body = call_tool_json(result);
    body["stores"]
        .as_array()
        .expect("stores array")
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn mcp_admin_sees_every_store_regardless_of_visibility() {
    let (_dir, state, app) = common::make_enforced_app().await;
    let admin_secret =
        common::seed_user_with_key(&state, "mcp-admin-vis", localdb_core::auth::Role::Admin).await;
    common::create_store_as(app.clone(), "shared-x", "shared", Some(&admin_secret)).await;
    common::create_store_as(app.clone(), "private-x", "private", Some(&admin_secret)).await;

    let (client, server_task) = connect_mcp_client(app, Some(&admin_secret)).await;

    let result = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("admin call_tool should succeed");
    let mut names = list_stores_names(result);
    names.sort();
    assert_eq!(names, vec!["private-x", "shared-x"]);

    let _ = client.cancel().await;
    server_task.abort();
}

#[tokio::test]
async fn mcp_member_sees_only_granted_shared_stores() {
    let (_dir, state, app) = common::make_enforced_app().await;
    let admin_secret =
        common::seed_user_with_key(&state, "mcp-admin-grant", localdb_core::auth::Role::Admin)
            .await;
    common::create_store_as(app.clone(), "granted", "shared", Some(&admin_secret)).await;
    common::create_store_as(app.clone(), "ungranted", "shared", Some(&admin_secret)).await;
    common::create_store_as(app.clone(), "private-z", "private", Some(&admin_secret)).await;

    let member = state
        .auth()
        .create_user("mcp-member", localdb_core::auth::Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    let resp = request_with_bearer_admin(
        &app,
        axum::http::Method::POST,
        "/v1/stores/granted/grants",
        Some(serde_json::json!({ "user": "mcp-member" })),
        &admin_secret,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let (client, server_task) = connect_mcp_client(app, Some(&member_secret)).await;

    let result = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .expect("member call_tool should succeed");
    assert_eq!(list_stores_names(result), vec!["granted"]);

    let _ = client.cancel().await;
    server_task.abort();
}

#[tokio::test]
async fn mcp_member_search_naming_an_invisible_store_is_forbidden() {
    // specs/05-surfaces.md §3.1: a store the member holds no grant for, but
    // that genuinely exists, is `forbidden` — not `store_not_found` — over
    // MCP, matching the HTTP `/v1/search` surface's identical
    // `store_filter` behavior (`search_service.rs`).
    let (_dir, state, app) = common::make_enforced_app().await;
    let admin_secret =
        common::seed_user_with_key(&state, "mcp-admin-search", localdb_core::auth::Role::Admin)
            .await;
    common::create_store_as(
        app.clone(),
        "invisible-to-member",
        "shared",
        Some(&admin_secret),
    )
    .await;

    let member = state
        .auth()
        .create_user("mcp-member-search", localdb_core::auth::Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;
    // No grant given: the store is invisible to this member.

    let (client, server_task) = connect_mcp_client(app, Some(&member_secret)).await;

    let args = serde_json::json!({ "query": "hello", "stores": ["invisible-to-member"] });
    let result = client
        .call_tool(
            CallToolRequestParams::new("search").with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("call_tool itself succeeds at the protocol level");
    assert_eq!(
        result.is_error,
        Some(true),
        "a named-but-unreadable store must be a tool-level error"
    );
    let body = call_tool_json(result);
    assert_eq!(body["error"]["code"], "forbidden");

    let _ = client.cancel().await;
    server_task.abort();
}

#[tokio::test]
async fn mcp_member_search_naming_a_truly_unknown_store_is_store_not_found() {
    // Only a name absent from the full store list stays `store_not_found` —
    // this is the counterpart to the `forbidden` case above.
    let (_dir, state, app) = common::make_enforced_app().await;
    let admin_secret = common::seed_user_with_key(
        &state,
        "mcp-admin-search-unknown",
        localdb_core::auth::Role::Admin,
    )
    .await;
    common::create_store_as(app.clone(), "granted-real", "shared", Some(&admin_secret)).await;

    let member = state
        .auth()
        .create_user(
            "mcp-member-search-unknown",
            localdb_core::auth::Role::Member,
        )
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    let resp = request_with_bearer_admin(
        &app,
        axum::http::Method::POST,
        "/v1/stores/granted-real/grants",
        Some(serde_json::json!({ "user": "mcp-member-search-unknown" })),
        &admin_secret,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let (client, server_task) = connect_mcp_client(app, Some(&member_secret)).await;

    let args = serde_json::json!({ "query": "hello", "stores": ["does-not-exist-at-all"] });
    let result = client
        .call_tool(
            CallToolRequestParams::new("search").with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("call_tool itself succeeds at the protocol level");
    assert_eq!(result.is_error, Some(true));
    let body = call_tool_json(result);
    assert_eq!(body["error"]["code"], "store_not_found");

    let _ = client.cancel().await;
    server_task.abort();
}

#[tokio::test]
async fn mcp_admin_search_naming_any_store_works() {
    // An admin is never subject to D7 filtering, so naming any real store —
    // private or shared — must succeed rather than error.
    let (_dir, state, app) = common::make_enforced_app().await;
    let admin_secret = common::seed_user_with_key(
        &state,
        "mcp-admin-search-any",
        localdb_core::auth::Role::Admin,
    )
    .await;
    common::create_store_as(app.clone(), "admin-private", "private", Some(&admin_secret)).await;

    let (client, server_task) = connect_mcp_client(app, Some(&admin_secret)).await;

    let args = serde_json::json!({ "query": "hello", "stores": ["admin-private"] });
    let result = client
        .call_tool(
            CallToolRequestParams::new("search").with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("call_tool itself succeeds at the protocol level");
    assert_ne!(
        result.is_error,
        Some(true),
        "admin naming any real store must succeed"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

#[tokio::test]
async fn mcp_grant_added_after_connect_is_visible_on_the_very_next_call() {
    let (_dir, state, app) = common::make_enforced_app().await;
    let admin_secret = common::seed_user_with_key(
        &state,
        "mcp-admin-realtime",
        localdb_core::auth::Role::Admin,
    )
    .await;
    common::create_store_as(
        app.clone(),
        "realtime-shared",
        "shared",
        Some(&admin_secret),
    )
    .await;

    let member = state
        .auth()
        .create_user("mcp-member-realtime", localdb_core::auth::Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    let (client, server_task) = connect_mcp_client(app.clone(), Some(&member_secret)).await;

    // Before the grant: invisible.
    let before = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .unwrap();
    assert!(list_stores_names(before).is_empty());

    // Grant it — no reconnect.
    let resp = request_with_bearer_admin(
        &app,
        axum::http::Method::POST,
        "/v1/stores/realtime-shared/grants",
        Some(serde_json::json!({ "user": "mcp-member-realtime" })),
        &admin_secret,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Same connected client, next call: visible.
    let after = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .unwrap();
    assert_eq!(list_stores_names(after), vec!["realtime-shared"]);

    let _ = client.cancel().await;
    server_task.abort();
}

#[tokio::test]
async fn mcp_grant_revoked_is_gone_on_the_very_next_call() {
    let (_dir, state, app) = common::make_enforced_app().await;
    let admin_secret =
        common::seed_user_with_key(&state, "mcp-admin-revoke", localdb_core::auth::Role::Admin)
            .await;
    common::create_store_as(
        app.clone(),
        "revocable-shared",
        "shared",
        Some(&admin_secret),
    )
    .await;

    let member = state
        .auth()
        .create_user("mcp-member-revoke", localdb_core::auth::Role::Member)
        .await
        .unwrap();
    let member_secret = state.auth().issue_api_key(&member.id).await.unwrap().secret;

    request_with_bearer_admin(
        &app,
        axum::http::Method::POST,
        "/v1/stores/revocable-shared/grants",
        Some(serde_json::json!({ "user": "mcp-member-revoke" })),
        &admin_secret,
    )
    .await;

    let (client, server_task) = connect_mcp_client(app.clone(), Some(&member_secret)).await;

    let before = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .unwrap();
    assert_eq!(list_stores_names(before), vec!["revocable-shared"]);

    // Revoke — no reconnect.
    request_with_bearer_admin(
        &app,
        axum::http::Method::DELETE,
        "/v1/stores/revocable-shared/grants/mcp-member-revoke",
        None,
        &admin_secret,
    )
    .await;

    let after = client
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await
        .unwrap();
    assert!(
        list_stores_names(after).is_empty(),
        "grant revocation must take effect on the very next call, no restart"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// Drive a plain `/v1` request against `app` via `oneshot` with an admin
/// bearer — a thin wrapper so the grant-toggling calls above don't need to
/// juggle a second real HTTP listener alongside the MCP client's.
async fn request_with_bearer_admin(
    app: &axum::Router,
    method: axum::http::Method,
    uri: &str,
    body: Option<Value>,
    bearer: &str,
) -> axum::response::Response {
    use tower::ServiceExt;

    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    builder = builder.header("authorization", format!("Bearer {bearer}"));
    let request_body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            axum::body::Body::from(value.to_string())
        }
        None => axum::body::Body::empty(),
    };
    app.clone()
        .oneshot(builder.body(request_body).unwrap())
        .await
        .unwrap()
}
