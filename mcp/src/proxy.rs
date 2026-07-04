//! `ProxyHandler` — forwards `tools/list`/`tools/call` verbatim to a running
//! daemon's `/mcp` HTTP route (Phase 3 scope, see `lib.rs`'s crate-level doc
//! comment).
//!
//! Every other `ServerHandler` in this crate (`handler::McpHandler`) is
//! macro-native: `#[tool_router]`/`#[tool_handler]` generates dispatch from
//! typed argument structs it owns ahead of time. `ProxyHandler` cannot be
//! macro-native — it has no argument structs of its own and does not know
//! the upstream's tool set ahead of time (that set is whatever store
//! snapshot the daemon happened to build at its own startup, see
//! `server::mcp_bridge::build_available_stores`'s doc comment) — so it just
//! relays whatever request arrives to the upstream connection and returns
//! whatever comes back, unexamined. This is the one hand-written
//! `ServerHandler` impl in the migration, deliberately.

use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleClient, RoleServer, RunningService, ServiceError},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ErrorData as McpError, ServerHandler, ServiceExt,
};

/// A `ServerHandler` that proxies every `tools/list`/`tools/call` request to
/// an upstream rmcp server reached over Streamable HTTP — used when
/// `localdb mcp` runs while a daemon is already up (see
/// `entrypoint::run_stdio`).
///
/// Holds the upstream MCP client session for the handler's whole lifetime:
/// `RunningService` owns the background task pumping the HTTP transport, so
/// keeping it as a field (rather than just a `Peer`) is what keeps that task
/// — and the upstream `initialize` handshake it already completed — alive
/// for as long as this stdio process serves requests.
pub struct ProxyHandler {
    upstream: RunningService<RoleClient, rmcp::model::ClientInfo>,
}

impl ProxyHandler {
    /// Connect to `{daemon_base_url}/mcp` and complete the upstream MCP
    /// `initialize` handshake.
    ///
    /// # Errors
    /// Returns an error if the HTTP transport cannot be constructed or the
    /// upstream handshake fails (e.g. the daemon went down between
    /// `probe_daemon` succeeding in `cli` and this call).
    pub async fn connect(daemon_base_url: &str) -> anyhow::Result<Self> {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("{daemon_base_url}/mcp")),
        );
        let upstream = rmcp::model::ClientInfo::default().serve(transport).await?;
        Ok(Self { upstream })
    }
}

/// Unwrap a `Peer<RoleClient>` call's `ServiceError` back into the tier the
/// upstream itself chose.
///
/// `ServiceError::McpError` is the upstream's own protocol-level `ErrorData`
/// — e.g. the "tool not found" error `handler::McpHandler`'s macro-generated
/// dispatch returns for an unregistered name (see `lib.rs`'s two-tier error
/// model doc) — and is forwarded unchanged so that tier survives the extra
/// hop. Any other `ServiceError` variant (transport closed, timeout, ...) is
/// a failure of the proxy hop itself, not a re-tiering of an upstream
/// answer: the upstream never got to answer at all, so there is no tier of
/// *its* to preserve. Those become a fresh protocol-level `internal_error`.
fn upstream_error_to_mcp(err: ServiceError) -> McpError {
    match err {
        ServiceError::McpError(e) => e,
        other => {
            McpError::internal_error(format!("mcp proxy: upstream request failed: {other}"), None)
        }
    }
}

impl ServerHandler for ProxyHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("localdb", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.upstream
            .list_tools(request)
            .await
            .map_err(upstream_error_to_mcp)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.upstream
            .call_tool(request)
            .await
            .map_err(upstream_error_to_mcp)
    }
}
