//! Protocol-level tests for the MCP server.
//!
//! These tests drive `McpHandler` over a real `rmcp` client/server pair
//! connected by an in-memory `tokio::io::duplex` — the same transport shape
//! a real stdio client would see, minus the OS pipe.
//!
//! Acceptance criteria (T10, carried over from the pre-rmcp suite):
//! - Tool list exactly the five read-only tools.
//! - `search` returns structured citations matching the canonical JSON.
//! - Unknown store name → `store_not_found` as MCP tool error.
//! - No mutating capability reachable.
//!
//! Two-tier error model (new in the rmcp migration, see `mcp/src/lib.rs` for
//! the full writeup — verified empirically here, not assumed):
//! - **Protocol-level** (`Err(ServiceError::McpError)`, `ErrorCode::INVALID_PARAMS`):
//!   only an unregistered tool *name* (`unknown_tool::test_unknown_tool_call`).
//! - **Tool-level** (`Ok(CallToolResult { is_error: Some(true), .. })`):
//!   everything else — including a missing/wrong-typed *required* argument.
//!   One might expect that to be a protocol-level error since `Parameters<T>`
//!   deserialization itself produces an `ErrorData::invalid_params`, but
//!   rmcp 1.8.0's `ToolRouter::call` downgrades any such error to a tool
//!   result via `into_tool_argument_error` (see `harness::assert_deserialization_error`
//!   and `search::test_search_missing_query_argument` /
//!   `get_document::test_get_document_no_args` /
//!   `get_chunks::test_get_chunks_missing_resource_id`).
//!
//! See specs/05-surfaces.md §4 and specs/02-domain-model.md §6.
//!
//! Split into one module per tool/concern (module-size rule) — this file
//! (`main.rs`, the crate root cargo discovers for the `mcp_protocol`
//! integration test binary) holds only the shared harness's `mod`
//! declaration; every `#[tokio::test]` lives in a sibling file.

mod anchor_pagination;
mod get_chunks;
mod get_document;
mod harness;
mod list_stores;
mod search;
mod tools_list;
mod unknown_tool;
