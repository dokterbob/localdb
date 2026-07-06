//! Stdio entrypoints for the MCP server: embedded (Phase 1) and
//! daemon-proxied (Phase 3, see `lib.rs`'s crate-level doc comment).
//!
//! No daemon probing happens in this crate at all — `cli::cmds::surface`
//! calls `cli`'s own `daemon_client::probe_daemon` (which this crate has no
//! dependency on — see `lib.rs`'s doc comment for why) and, depending on the
//! result, calls either [`serve_embedded_stdio`] directly or
//! `proxy::ProxyHandler::connect` followed by [`serve_proxied_stdio`] — the
//! two entrypoints `cli` calls going forward. Keeping them as two separate
//! calls (rather than one moded entrypoint) lets the caller distinguish a
//! failed daemon *connection* from a failure in the stdio-serving loop
//! itself, which matters for mapping to distinct stable exit codes
//! (`daemon_unreachable` vs `internal`).

use rmcp::{service::ServerInitializeError, ServerHandler, ServiceExt};

use crate::handler::McpHandler;
use crate::proxy::ProxyHandler;

/// Serve an already-connected `ProxyHandler` over stdio until the client
/// disconnects.
///
/// Kept separate from `ProxyHandler::connect` (rather than one function
/// doing both) so `cli::cmds::surface::run_mcp_async` can map a failure to
/// connect (daemon gone, stale `LOCALDB_DAEMON_URL`) to `daemon_unreachable`,
/// while a failure in this loop — a much rarer case, since the upstream
/// handshake already succeeded — maps to a plain internal error instead.
///
/// # Errors
/// Returns an error if the transport fails or the service loop errors while
/// running.
pub async fn serve_proxied_stdio(handler: ProxyHandler) -> anyhow::Result<()> {
    serve_stdio(handler).await
}

/// Serve the given handler over stdio until the client disconnects.
///
/// # Errors
/// Returns an error if the transport fails to initialize (other than the
/// client disconnecting before ever sending `initialize` — see below) or
/// the service loop errors while running.
pub async fn serve_embedded_stdio(handler: McpHandler) -> anyhow::Result<()> {
    serve_stdio(handler).await
}

/// Shared stdio-serving loop for any `ServerHandler` — both `McpHandler`
/// (embedded) and `ProxyHandler` (daemon-delegated) hit the same stdin-EOF
/// special case below, so it is factored out once rather than duplicated
/// per run mode.
async fn serve_stdio<H: ServerHandler>(handler: H) -> anyhow::Result<()> {
    let service = match handler.serve(rmcp::transport::stdio()).await {
        Ok(service) => service,
        // stdin closed (EOF) before any `initialize` request ever arrived —
        // e.g. `localdb mcp < /dev/null`, or a health check that just probes
        // the process starts and exits. The pre-rmcp hand-rolled stdio loop
        // treated stdin EOF as a clean shutdown unconditionally, regardless
        // of handshake state; rmcp's own `serve()` instead surfaces this as
        // `ServerInitializeError::ConnectionClosed`. Preserve the old
        // behavior — this is not an operator-visible failure — rather than
        // exiting non-zero (see `localdb/tests/cli_integration.rs`'s
        // `mcp_exits_cleanly_on_stdin_eof`).
        Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    service.waiting().await?;
    Ok(())
}
