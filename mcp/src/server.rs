//! MCP server main loop — reads JSON-RPC messages from stdin, writes to stdout.
//!
//! The server is single-threaded from the message-dispatch perspective:
//! one message at a time, no concurrency between dispatches. Async is used
//! only for the underlying store/embedder calls.
//!
//! See specs/05-surfaces.md §4.

use std::io::{self, BufRead, Write};

use serde_json::Value;

use localdb_core::Embedder;

use crate::protocol::{
    CallToolResult, InitializeResult, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    ServerCapabilities, ServerInfo, Tool, ToolsCapability, ToolsListResult, INVALID_PARAMS,
    METHOD_NOT_FOUND,
};
use crate::tools::{tool_get_document, tool_list_stores, tool_search, AvailableStore};

/// MCP protocol version this server implements.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Tool names.
pub const TOOL_SEARCH: &str = "search";
pub const TOOL_GET_DOCUMENT: &str = "get_document";
pub const TOOL_LIST_STORES: &str = "list_stores";

// ---------------------------------------------------------------------------
// Tool schema definitions (JSON Schema)
// ---------------------------------------------------------------------------

fn search_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Natural language search query"
            },
            "stores": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional list of store names to search. Defaults to all stores."
            },
            "limit": {
                "type": "integer",
                "description": "Maximum number of results to return (default: 10, max: 100)",
                "minimum": 1,
                "maximum": 100
            }
        },
        "required": ["query"]
    })
}

fn get_document_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Document ID (content-addressed blake3 hash)"
            },
            "uri": {
                "type": "string",
                "description": "Document URI (e.g. file:///path/to/doc or URL)"
            }
        }
    })
}

fn list_stores_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {}
    })
}

/// Build the list of tools this server exposes.
pub fn build_tool_list() -> Vec<Tool> {
    vec![
        Tool {
            name: TOOL_SEARCH,
            description: "Hybrid search (BM25 + dense vector) across indexed stores. Returns a ranked list of citations in the canonical localdb Citation JSON shape.",
            input_schema: search_schema(),
        },
        Tool {
            name: TOOL_GET_DOCUMENT,
            description: "Fetch the normalized text and metadata for a document by its ID or URI.",
            input_schema: get_document_schema(),
        },
        Tool {
            name: TOOL_LIST_STORES,
            description: "List all available stores with their names, visibility, and document/chunk counts.",
            input_schema: list_stores_schema(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

/// Shared server state passed to the message handler.
pub struct McpServer {
    pub stores: Vec<AvailableStore>,
    pub embedder: Box<dyn Embedder>,
    /// Whether --allow-write was passed (always rejected in v1).
    pub allow_write: bool,
}

impl McpServer {
    /// Create a new server.
    pub fn new(stores: Vec<AvailableStore>, embedder: Box<dyn Embedder>) -> Self {
        Self {
            stores,
            embedder,
            allow_write: false,
        }
    }

    /// Handle a single JSON-RPC request and produce a response value to send.
    ///
    /// Returns `None` for notifications (no response expected).
    pub async fn handle_message(&self, req: &JsonRpcRequest) -> Option<String> {
        // Notifications (no id) get no response.
        if req.is_notification() {
            // Still handle side effects (e.g. initialized).
            return None;
        }

        let id = req.id.clone().unwrap_or(Value::Null);

        let response_str = match req.method.as_str() {
            "initialize" => self.handle_initialize(id),
            "tools/list" => self.handle_tools_list(id),
            "tools/call" => self.handle_tools_call(id, req.params.as_ref()).await,
            "ping" => {
                let resp = JsonRpcResponse::ok(id, serde_json::json!({}));
                serde_json::to_string(&resp).unwrap_or_default()
            }
            method => {
                let err =
                    JsonRpcError::new(id, METHOD_NOT_FOUND, format!("method not found: {method}"));
                serde_json::to_string(&err).unwrap_or_default()
            }
        };

        Some(response_str)
    }

    fn handle_initialize(&self, id: Value) -> String {
        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION,
            capabilities: ServerCapabilities {
                tools: ToolsCapability {
                    list_changed: false,
                },
            },
            server_info: ServerInfo {
                name: "localdb",
                version: env!("CARGO_PKG_VERSION"),
            },
        };
        let resp = JsonRpcResponse::ok(id, serde_json::to_value(&result).unwrap_or_default());
        serde_json::to_string(&resp).unwrap_or_default()
    }

    fn handle_tools_list(&self, id: Value) -> String {
        let result = ToolsListResult {
            tools: build_tool_list(),
        };
        let resp = JsonRpcResponse::ok(id, serde_json::to_value(&result).unwrap_or_default());
        serde_json::to_string(&resp).unwrap_or_default()
    }

    async fn handle_tools_call(&self, id: Value, params: Option<&Value>) -> String {
        let tool_name = params.and_then(|p| p.get("name")).and_then(|n| n.as_str());

        let tool_name = match tool_name {
            Some(n) => n,
            None => {
                let err = JsonRpcError::new(id, INVALID_PARAMS, "missing tool name in params");
                return serde_json::to_string(&err).unwrap_or_default();
            }
        };

        let result: CallToolResult = match tool_name {
            TOOL_SEARCH => tool_search(&self.stores, self.embedder.as_ref(), params).await,
            TOOL_GET_DOCUMENT => tool_get_document(&self.stores, params).await,
            TOOL_LIST_STORES => tool_list_stores(&self.stores).await,
            name => CallToolResult::error(format!(
                "unknown tool '{name}'; available: {TOOL_SEARCH}, {TOOL_GET_DOCUMENT}, {TOOL_LIST_STORES}"
            )),
        };

        let resp = JsonRpcResponse::ok(id, serde_json::to_value(&result).unwrap_or_default());
        serde_json::to_string(&resp).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Stdio message loop
// ---------------------------------------------------------------------------

/// Run the MCP server loop: read lines from stdin, write responses to stdout.
///
/// This function blocks until stdin is closed.
///
/// # Errors
/// Returns an `io::Error` if reading/writing the stdio streams fails.
pub async fn run_stdio_loop(server: &McpServer) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout_lock = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                // Parse error: send a JSON-RPC parse error back.
                let err = JsonRpcError::new(
                    Value::Null,
                    crate::protocol::PARSE_ERROR,
                    format!("parse error: {e}"),
                );
                let resp_str = serde_json::to_string(&err).unwrap_or_default();
                writeln!(stdout_lock, "{resp_str}")?;
                stdout_lock.flush()?;
                continue;
            }
        };

        if let Some(response) = server.handle_message(&req).await {
            writeln!(stdout_lock, "{response}")?;
            stdout_lock.flush()?;
        }
    }

    Ok(())
}

/// Parse a raw JSON-RPC line into a `JsonRpcRequest`.
pub fn parse_message(line: &str) -> Result<JsonRpcRequest, String> {
    serde_json::from_str(line).map_err(|e| e.to_string())
}
