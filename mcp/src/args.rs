//! Typed argument structs for MCP tool calls.
//!
//! Schema is derived from these types via `schemars` — replacing the old
//! hand-written JSON-schema builder functions and `*Args::from_value` raw
//! `serde_json::Value` parsers. rmcp's `Parameters<T>` extractor deserializes
//! the incoming `tools/call` `arguments` object into these structs before a
//! tool method body ever runs.
//!
//! A missing/wrong-typed *required* field (a non-`Option` field: `query` on
//! [`SearchArgs`], `id` on [`GetDocumentArgs`], `resource_id` on
//! [`GetChunksArgs`]) fails at this deserialization step — but that surfaces
//! as a **tool-level** `CallToolResult` error (rmcp downgrades it via
//! `into_tool_argument_error`; it does *not* propagate as a protocol-level
//! JSON-RPC error), with a generic rmcp-authored "failed to deserialize
//! parameters: ..." message rather than our own structured `typed_error`
//! JSON. All other validation (ranges, non-empty strings, cross-store
//! checks) is semantic and stays in the tool bodies in `tools.rs`, reading
//! from these already-typed structs — see the crate-level doc comment in
//! `lib.rs` for the full two-tier model and how it was verified.

use schemars::JsonSchema;
use serde::Deserialize;

/// Arguments for the `search` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Natural language search query. Missing or non-string input fails
    /// deserialization (a tool-level "failed to deserialize parameters"
    /// error, see `lib.rs`); an empty/whitespace-only string is a
    /// tool-level `invalid_request` error (checked in `tools::tool_search`).
    #[schemars(description = "Natural language search query")]
    pub query: String,

    /// Optional list of store names to search. Defaults to all stores.
    #[serde(default)]
    #[schemars(description = "Optional list of store names to search. Defaults to all stores.")]
    pub stores: Option<Vec<String>>,

    /// Maximum number of results to return (default: 10, max: 100).
    #[serde(default)]
    #[schemars(
        description = "Maximum number of results to return (default: 10, max: 100)",
        range(min = 1, max = 100)
    )]
    pub limit: Option<i64>,

    /// Soft cap on snippet text chars per result in the text rendering; snaps
    /// to the nearest paragraph/sentence/word boundary rather than cutting
    /// mid-word (default: 400). The JSON citation payload always carries the
    /// full snippet.
    #[serde(default)]
    #[schemars(
        description = "Soft cap on snippet text chars per result in the text rendering; snaps to the nearest paragraph/sentence/word boundary rather than cutting mid-word (default: 400). The JSON citation payload always carries the full snippet.",
        range(min = 1)
    )]
    pub content_length: Option<i64>,
}

/// Arguments for the `get_document` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDocumentArgs {
    /// Document ID (content-addressed blake3 hash). `#[serde(default)]`
    /// (empty string) rather than a hard-required field: a caller who omits
    /// `id` entirely (e.g. a `uri`-only call) must still reach
    /// `tools::tool_get_document`'s body, which distinguishes "empty `id`,
    /// no `uri`" from "empty `id`, `uri` given" to produce its
    /// v1-guidance-specific `invalid_request` message. Making `id` a hard
    /// schema-required field would instead fail *all* omitted-`id` calls at
    /// deserialization with rmcp's generic message, silently losing that
    /// guidance for the `uri`-only case. Non-string input still fails
    /// deserialization either way.
    #[serde(default)]
    #[schemars(description = "Document ID (content-addressed blake3 hash)")]
    pub id: String,

    /// Document URI (e.g. file:///path/to/doc or URL). Acknowledged but not
    /// supported in v1 — `tools::tool_get_document` rejects it with a helpful
    /// message pointing the caller at `id`.
    #[serde(default)]
    #[schemars(description = "Document URI (e.g. file:///path/to/doc or URL)")]
    pub uri: Option<String>,
}

/// Arguments for the `get_chunks` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetChunksArgs {
    /// Resource ID (content-addressed blake3 hash). Missing or non-string
    /// input fails deserialization (a tool-level "failed to deserialize
    /// parameters" error, see `lib.rs`).
    #[schemars(description = "Resource ID (content-addressed blake3 hash)")]
    pub resource_id: String,

    /// Number of chunks to skip before the first returned chunk (default: 0).
    #[serde(default)]
    #[schemars(
        description = "Number of chunks to skip before the first returned chunk (default: 0)",
        range(min = 0)
    )]
    pub offset: Option<i64>,

    /// Maximum number of chunks to return (default: 50, max: 200).
    #[serde(default)]
    #[schemars(
        description = "Maximum number of chunks to return (default: 50, max: 200)",
        range(min = 1, max = 200)
    )]
    pub limit: Option<i64>,
}
