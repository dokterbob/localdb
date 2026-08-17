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

use localdb_core::{Embedder, StoreBackend};

use crate::handler::McpHandler;
use crate::tools::AvailableStore;

/// Build the Streamable HTTP tower service serving `McpHandler`.
///
/// `stores`, `backend`, and `embedder` are a startup-time snapshot (see
/// `server::mcp_bridge::build_available_stores`), not rebuilt per session:
/// rmcp's service factory below is a synchronous `Fn() -> Result<S,
/// io::Error>`, so there is no hook to redo the async `AppState` lookups
/// per HTTP session. The factory clones `stores`/`backend`/`embedder` per
/// session instead — cheap, since `AvailableStore::store`, `backend`, and
/// `embedder` are all already `Arc`-backed — which satisfies the sync
/// boundary without a `block_on` bridge. A store added later via `/v1/stores`
/// is therefore
/// invisible over MCP until the daemon restarts; an accepted, documented
/// gap (specs/05-surfaces.md §4), not something this function works around.
///
/// HTTP MCP sessions always run with `allow_write = false`: there is no
/// CLI-flag equivalent for an HTTP caller, and v1 registers no mutating
/// tool regardless (see `handler.rs`'s doc comment).
///
/// `allowed_hosts` feeds rmcp's own DNS-rebinding `Host`-header allowlist
/// (`StreamableHttpServerConfig::allowed_hosts`). rmcp's *default* for that
/// list is `localhost`/`127.0.0.1`/`::1` only — narrower than, and
/// independent of, the daemon's own non-loopback-bind trust decision
/// (`server::daemon::warn_if_unspecified`, PR #135): a deliberately-bound
/// LAN/Tailscale address works for every other `/v1` route but rmcp still
/// 403s `/mcp` with "Host header is not allowed", which MCP clients (e.g.
/// Claude Code) surface as a spurious "needs authentication". The caller
/// (`server::daemon::build_router`, via `mcp_allowed_hosts`) computes this
/// list from the daemon's *actually bound* address, so it must be threaded
/// straight through here rather than falling back to
/// `StreamableHttpServerConfig::default()`'s own opinion.
///
/// An empty `allowed_hosts` means the caller decided the check should be
/// disabled entirely (a wildcard `0.0.0.0`/`::` bind, which already accepts
/// connections from any network — there's no single external host to list,
/// and a partial Host check on top of an already-fully-open bind would be
/// inconsistent, not more secure). Note this is *not* the same as leaving
/// the field at its default: `StreamableHttpServerConfig::default()`'s own
/// `allowed_hosts` is non-empty, so an empty input here must explicitly
/// call `.disable_allowed_hosts()` to clear it — otherwise the wildcard-bind
/// case would silently keep rmcp's localhost-only default and this whole
/// fix would be a no-op for the one case (non-loopback bind) it exists for.
/// Do not "simplify" this back to always using `::default()`.
pub fn build_streamable_http_service(
    stores: Vec<AvailableStore>,
    backend: Arc<dyn StoreBackend>,
    embedder: Arc<dyn Embedder>,
    allowed_hosts: Vec<String>,
) -> StreamableHttpService<McpHandler, LocalSessionManager> {
    let config = if allowed_hosts.is_empty() {
        StreamableHttpServerConfig::default().disable_allowed_hosts()
    } else {
        StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts)
    };
    StreamableHttpService::new(
        move || {
            Ok(McpHandler::new(
                stores.clone(),
                backend.clone(),
                embedder.clone(),
                false,
            ))
        },
        Default::default(),
        config,
    )
}
