//! MCP server for localdb.
//!
//! Stdio MCP server exposing read-only tools:
//! - `search`: hybrid search returning Citation list (canonical JSON shape)
//! - `get_document`: fetch normalized text + metadata by id
//! - `get_chunks`: fetch a document's chunks in order, paginated
//! - `list_stores`: names, visibility, chunk/document counts
//!
//! ## Implementation
//! Built on the official [`rmcp`](https://docs.rs/rmcp) SDK: `McpHandler`
//! (`handler.rs`) is a `#[tool_router]`/`#[tool_handler]` `ServerHandler` —
//! there is no hand-written JSON-RPC dispatch or JSON-schema builder.
//! Argument schemas (`args.rs`) are derived from typed structs via
//! `schemars`; rmcp's `Parameters<T>` extractor deserializes each
//! `tools/call` request into one of these structs before the corresponding
//! `#[tool]` method runs.
//!
//! ## Two-tier error model
//! There are exactly two failure tiers, split by whether the request could
//! be *routed* to a tool at all:
//!
//! - **Protocol-level** (`Err(ErrorData)`, a JSON-RPC error): the tool name
//!   itself is unregistered. rmcp's macro-generated `call_tool` returns
//!   `ErrorData::invalid_params("tool not found", None)` for any name not in
//!   the `#[tool_router]` table (verified against rmcp 1.8.0 — see
//!   `handler/server/router/tool.rs`'s `ToolRouter::call`). This is the one
//!   case a caller cannot recover from within the tool result.
//! - **Tool-level** (`Ok(CallToolResult { is_error: Some(true), .. })`):
//!   everything else, including cases one might expect to be
//!   protocol-level. This covers:
//!   - A missing or wrong-typed *required* argument (`search`'s `query`,
//!     `get_document`'s `id`, `get_chunks`'s `document_id`). One might
//!     expect `Parameters<T>`'s deserialization failure — itself an
//!     `ErrorData::invalid_params` — to propagate as a protocol error, but
//!     rmcp's `ToolRouter::call` special-cases it: any `ErrorData` whose
//!     message starts with `"failed to deserialize parameters:"` is
//!     downgraded to `Ok(CallToolResult::error(..))` via
//!     `into_tool_argument_error`, so the caller's MCP client can render it
//!     like any other tool result. This is a real behavior difference from
//!     what an initial reading of the rmcp source (or older SDK versions)
//!     might suggest — confirmed with a duplex-transport integration test
//!     in `mcp/tests/mcp_protocol.rs`, not assumed.
//!   - Our own semantic/business validation (empty strings, out-of-range
//!     `limit`/`offset`), unknown store names, not-found lookups, and the
//!     cross-store security check in `get_chunks`/`get_document` — all
//!     constructed via `tools::typed_error`, carrying a
//!     `{"error": {"code", "message"}}` JSON body as its text content.
//!
//! See `tools.rs` for the validation logic (largely unchanged from the
//! pre-rmcp implementation) and `args.rs` for the required/optional split
//! each tool's argument struct encodes — required-ness now only controls
//! whether a bad argument's error message is rmcp's generic "failed to
//! deserialize parameters: ..." or our own structured `typed_error` JSON,
//! not which tier the error surfaces at.
//!
//! ## Process model (Phase 1: stdio, Phase 2: HTTP mount, Phase 3: proxying)
//! `entrypoint.rs`'s `serve_embedded_stdio` always opens the store
//! in-process (embedded mode) over stdio. Phase 2 (`http.rs`) adds a second
//! transport: `server::daemon::build_router` mounts
//! `build_streamable_http_service`'s tower service at `/mcp` alongside the
//! daemon's own `/v1` routes, over a startup-time snapshot of stores (see
//! `http.rs`'s doc comment for why it isn't rebuilt per session). See
//! specs/05-surfaces.md §4 and specs/01-architecture.md §3. The
//! `--allow-write` flag is parsed but always rejected in v1: no mutating
//! tool is registered on either transport.
//!
//! Phase 3 (`entrypoint::serve_proxied_stdio`, `proxy.rs`) adds a third mode
//! rather than a third transport: `localdb mcp` still always speaks stdio to
//! its caller, but when `cli` detects a daemon is already running, it calls
//! `proxy::ProxyHandler::connect` and hands the result to
//! `entrypoint::serve_proxied_stdio` instead of building an embedded
//! `McpHandler`. In that mode, every stdio request is forwarded verbatim to
//! the daemon's own `/mcp` HTTP route by `proxy::ProxyHandler` — a
//! hand-written `ServerHandler` (the one non-macro-native one in this
//! crate; see its doc comment for why) — rather than opening the store a
//! second time in the CLI process. Store resolution then happens entirely
//! on the daemon side: a stdio caller's `--store` flags are not honored in
//! proxied mode (specs/05-surfaces.md §4 documents this as a known v1
//! gap), since the daemon's own `/mcp` route has no notion of a per-session
//! store filter to apply. `cli` alone decides which mode to run in — this
//! crate has no dependency on `cli` and never probes for a daemon itself.

pub mod args;
pub mod entrypoint;
pub mod handler;
pub mod http;
pub mod proxy;
pub mod tools;

// Re-export key items for the binary entry point and for `server`'s HTTP mount.
pub use entrypoint::{serve_embedded_stdio, serve_proxied_stdio};
pub use handler::McpHandler;
pub use http::build_streamable_http_service;
pub use tools::{AvailableStore, StoreDescriptor};
