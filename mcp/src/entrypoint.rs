//! Stdio entrypoints for the MCP server: embedded (Phase 1) and
//! daemon-proxied (Phase 3, see `lib.rs`'s crate-level doc comment).
//!
//! No daemon probing happens in this crate at all — `cli::cmds::surface`
//! resolves an [`McpRunMode`] (by calling `cli`'s own
//! `daemon_client::probe_daemon`, which this crate has no dependency on —
//! see `lib.rs`'s doc comment for why) and hands the already-resolved mode
//! to [`run_stdio`], the sole entrypoint `cli` calls going forward.

use rmcp::{service::ServerInitializeError, ServerHandler, ServiceExt};

use crate::handler::McpHandler;
use crate::proxy::ProxyHandler;

/// Which backend `localdb mcp` serves over stdio.
pub enum McpRunMode {
    /// Store(s) opened in-process — today's (Phase 1/2) behavior.
    Embedded(McpHandler),
    /// No local store access at all: every request is forwarded to a
    /// running daemon's `/mcp` HTTP route (see `proxy::ProxyHandler`).
    Proxied { daemon_base_url: String },
}

/// Serve MCP over stdio using the resolved run mode.
///
/// `cli::cmds::surface::run_mcp_async` is the sole caller: it probes for a
/// running daemon first and only ever constructs the `Embedded` variant
/// (which requires opening the store) when there is none.
///
/// # Errors
/// Returns an error if `Proxied` mode fails to connect to the daemon, or the
/// stdio service loop errors while running (see `serve_stdio`).
pub async fn run_stdio(mode: McpRunMode) -> anyhow::Result<()> {
    match mode {
        McpRunMode::Embedded(handler) => serve_embedded_stdio(handler).await,
        McpRunMode::Proxied { daemon_base_url } => {
            let handler = ProxyHandler::connect(&daemon_base_url).await?;
            serve_proxied_stdio(handler).await
        }
    }
}

/// Serve an already-connected `ProxyHandler` over stdio until the client
/// disconnects.
///
/// Split out from `run_stdio`'s `Proxied` arm (rather than only reachable
/// through it) so `cli::cmds::surface::run_mcp_async` can call
/// `ProxyHandler::connect` and this separately: a failure to connect (daemon
/// gone, stale `LOCALDB_DAEMON_URL`) is a `daemon_unreachable` condition,
/// while a failure in this loop is a Phase-3-proxy-specific internal error —
/// `run_stdio`'s single `anyhow::Result` return can't tell those apart for a
/// caller that wants to map them to different stable exit codes.
///
/// # Errors
/// Returns an error if the transport fails or the service loop errors while
/// running.
pub async fn serve_proxied_stdio(handler: ProxyHandler) -> anyhow::Result<()> {
    serve_stdio(handler).await
}

/// Serve the given handler over stdio until the client disconnects.
///
/// Kept as its own public function (rather than folded entirely into
/// `run_stdio`) since Phase 1/2 code and docs already call it by this name
/// directly; `run_stdio` is the primary entrypoint for `cli` going forward,
/// but this remains a valid, supported way to serve an already-built
/// `McpHandler` embedded.
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
