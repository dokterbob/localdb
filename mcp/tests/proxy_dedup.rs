use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{
    model::*,
    service::{RequestContext, RoleServer},
    ErrorData, ServerHandler, ServiceExt,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone)]
struct Upstream {
    behavior: &'static str,
    lists: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}
impl ServerHandler for Upstream {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let attempt = self.lists.fetch_add(1, Ordering::SeqCst);
        if self.behavior == "retry" && attempt == 0 {
            return Err(ErrorData::invalid_request("discovery failed", None));
        }
        if self.behavior == "cycle"
            || (self.behavior == "paged" && request.and_then(|r| r.cursor).is_none())
        {
            return Ok(ListToolsResult {
                next_cursor: Some("next".to_string()),
                ..Default::default()
            });
        }
        let props = if self.behavior == "old" {
            serde_json::json!({})
        } else {
            serde_json::json!({"dedup":{"type":"string"}})
        };
        let tool: Tool = serde_json::from_value(
            serde_json::json!({"name":"search","inputSchema":{"type":"object","properties":props}}),
        )
        .unwrap();
        Ok(ListToolsResult {
            tools: vec![tool],
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name == "list_stores" {
            return Ok(CallToolResult::success(vec![Content::text(serde_json::json!({"stores":[{"id":"allowed", "name":"allowed"}, {"id":"denied", "name":"denied"}]}).to_string())]));
        }
        if let Some(stores) = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("stores"))
        {
            assert_eq!(stores, &serde_json::json!(["allowed"]));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CallToolResult::success(vec![Content::text("relayed")]))
    }
}

async fn connect(
    behavior: &'static str,
) -> (
    rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    connect_scoped(behavior, &[]).await
}

async fn connect_scoped(
    behavior: &'static str,
    scope: &[String],
) -> (
    rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let lists = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let upstream = Upstream {
        behavior,
        lists: lists.clone(),
        calls: calls.clone(),
    };
    let service = StreamableHttpService::<
        _,
        rmcp::transport::streamable_http_server::session::local::LocalSessionManager,
    >::new(
        move || Ok(upstream.clone()),
        Default::default(),
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
            .await
            .unwrap();
    });
    let proxy = mcp::proxy::ProxyHandler::connect(&format!("http://{address}"), scope)
        .await
        .unwrap();
    let (server, client) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        proxy.serve(server).await.unwrap().waiting().await.unwrap();
    });
    (().serve(client).await.unwrap(), lists, calls)
}
fn request(mode: Option<serde_json::Value>) -> CallToolRequestParams {
    let mut args = serde_json::json!({"query":"x"});
    if let Some(mode) = mode {
        args["dedup"] = mode;
    }
    CallToolRequestParams::new("search").with_arguments(args.as_object().unwrap().clone())
}
fn code(result: &CallToolResult) -> String {
    serde_json::from_str::<serde_json::Value>(&result.content[0].as_text().unwrap().text).unwrap()
        ["error"]["code"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn proxy_validates_before_discovery_and_off_bypasses_old_daemon() {
    let (client, lists, calls) = connect("old").await;
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!(true),
        serde_json::json!("bad"),
    ] {
        assert_eq!(
            code(&client.call_tool(request(Some(invalid))).await.unwrap()),
            "invalid_request"
        );
    }
    assert_eq!(lists.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_ne!(
        client
            .call_tool(request(Some(serde_json::json!("off"))))
            .await
            .unwrap()
            .is_error,
        Some(true)
    );
    assert_eq!(lists.load(Ordering::SeqCst), 0);
    for _ in 0..2 {
        assert_eq!(
            code(&client.call_tool(request(None)).await.unwrap()),
            "daemon_capability_unavailable"
        );
    }
    assert_eq!(lists.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn proxy_follows_pages_and_caches_supported_contract() {
    let (client, lists, calls) = connect("paged").await;
    for mode in [
        None,
        Some(serde_json::json!("text")),
        Some(serde_json::json!("text_and_vector")),
    ] {
        assert_ne!(
            client.call_tool(request(mode)).await.unwrap().is_error,
            Some(true)
        );
    }
    assert_eq!(lists.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn proxy_does_not_cache_discovery_errors_and_preserves_error_tier() {
    let (client, lists, calls) = connect("retry").await;
    let error = client.call_tool(request(None)).await.unwrap_err();
    assert!(
        matches!(error, rmcp::service::ServiceError::McpError(ref e) if e.message == "discovery failed")
    );
    assert_ne!(
        client.call_tool(request(None)).await.unwrap().is_error,
        Some(true)
    );
    assert_eq!(lists.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn proxy_rejects_repeated_cursors_without_caching_failure() {
    let (client, lists, calls) = connect("cycle").await;
    for _ in 0..2 {
        assert!(client
            .call_tool(request(None))
            .await
            .unwrap_err()
            .to_string()
            .contains("repeated"));
    }
    assert_eq!(lists.load(Ordering::SeqCst), 4);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn scoped_proxy_validates_modes_before_old_daemon_dispatch() {
    let (client, lists, calls) = connect_scoped("old", &["allowed".into()]).await;
    for invalid in [
        serde_json::Value::Null,
        serde_json::json!(1),
        serde_json::json!("unknown"),
    ] {
        assert_eq!(
            code(&client.call_tool(request(Some(invalid))).await.unwrap()),
            "invalid_request"
        );
    }
    assert_eq!(lists.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_ne!(
        client
            .call_tool(request(Some(serde_json::json!("off"))))
            .await
            .unwrap()
            .is_error,
        Some(true)
    );
    assert_eq!(lists.load(Ordering::SeqCst), 0);
    for mode in [
        None,
        Some(serde_json::json!("text")),
        Some(serde_json::json!("text_and_vector")),
    ] {
        assert_eq!(
            code(&client.call_tool(request(mode)).await.unwrap()),
            "daemon_capability_unavailable"
        );
    }
    assert_eq!(lists.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
