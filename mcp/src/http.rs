//! HTTP mount point for `McpHandler`, built on rmcp's Streamable HTTP
//! server transport (Phase 2 scope, see `lib.rs`'s crate-level doc comment).
//!
//! All rmcp-specific transport wiring lives here so `server` (the HTTP
//! daemon crate that mounts this alongside its own `/v1` routes) never has
//! to name an rmcp type directly — it just gets back a concrete
//! `tower::Service` to pass to `axum::Router::nest_service`.

use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use localdb_core::Embedder;

use crate::handler::McpHandler;
use crate::tools::AvailableStore;

/// Build the Streamable HTTP tower service serving `McpHandler`.
///
/// `stores` and `embedder` are a startup-time snapshot (see
/// `server::mcp_bridge::build_available_stores`), not rebuilt per session:
/// rmcp's service factory below is a synchronous `Fn() -> Result<S,
/// io::Error>`, so there is no hook to redo the async `AppState` lookups
/// per HTTP session. The factory clones `stores`/`embedder` per session
/// instead — cheap, since `AvailableStore::store` and `embedder` are both
/// already `Arc`-backed — which satisfies the sync boundary without a
/// `block_on` bridge. A store added later via `/v1/stores` is therefore
/// invisible over MCP until the daemon restarts; an accepted, documented
/// gap (specs/05-surfaces.md §4), not something this function works around.
///
/// HTTP MCP sessions always run with `allow_write = false`: there is no
/// CLI-flag equivalent for an HTTP caller, and v1 registers no mutating
/// tool regardless (see `handler.rs`'s doc comment).
pub fn build_streamable_http_service(
    stores: Vec<AvailableStore>,
    embedder: Arc<dyn Embedder>,
) -> StreamableHttpService<McpHandler, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(McpHandler::new(stores.clone(), embedder.clone(), false)),
        Default::default(),
        StreamableHttpServerConfig::default(),
    )
}
