//! MCP protocol types — JSON-RPC 2.0 messages and MCP-specific structures.
//!
//! Implements the Model Context Protocol over stdio.
//! Messages are newline-delimited JSON (one message per line).
//!
//! See specs/05-surfaces.md §4.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request (may be a request or a notification).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    /// Always "2.0".
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Parameters (optional).
    #[serde(default)]
    pub params: Option<Value>,
    /// Request ID — absent on notifications.
    #[serde(default)]
    pub id: Option<Value>,
}

impl JsonRpcRequest {
    /// Returns true if this is a notification (no id).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A JSON-RPC 2.0 success response.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    /// Always "2.0".
    pub jsonrpc: &'static str,
    /// The result value.
    pub result: Value,
    /// Matches the request id.
    pub id: Value,
}

impl JsonRpcResponse {
    /// Create a new success response.
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            result,
            id,
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    /// Always "2.0".
    pub jsonrpc: &'static str,
    /// Error details.
    pub error: RpcError,
    /// Matches the request id.
    pub id: Value,
}

impl JsonRpcError {
    /// Create a new error response.
    pub fn new(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            error: RpcError {
                code,
                message: message.into(),
                data: None,
            },
            id,
        }
    }

    /// Create a new error response with additional data.
    pub fn with_data(id: Value, code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            error: RpcError {
                code,
                message: message.into(),
                data: Some(data),
            },
            id,
        }
    }
}

/// Error detail within a JSON-RPC 2.0 error response.
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    /// Standard JSON-RPC error code.
    pub code: i32,
    /// Human-readable message.
    pub message: String,
    /// Optional additional data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;

// ---------------------------------------------------------------------------
// MCP-specific types
// ---------------------------------------------------------------------------

/// MCP `initialize` response result.
#[derive(Debug, Clone, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'static str,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
}

/// Server capabilities advertised during initialization.
#[derive(Debug, Clone, Serialize)]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
}

/// Tool capability declaration.
#[derive(Debug, Clone, Serialize)]
pub struct ToolsCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

/// Server identification info.
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// MCP tool definition.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// MCP `tools/list` response result.
#[derive(Debug, Clone, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<Tool>,
}

/// MCP content item — carries structured + text rendering.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ContentItem {
    #[serde(rename = "text")]
    Text { text: String },
}

/// MCP `tools/call` response result.
#[derive(Debug, Clone, Serialize)]
pub struct CallToolResult {
    pub content: Vec<ContentItem>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

impl CallToolResult {
    /// Success result with JSON content.
    pub fn success_json(value: &Value) -> Self {
        Self {
            content: vec![ContentItem::Text {
                text: serde_json::to_string_pretty(value).unwrap_or_default(),
            }],
            is_error: false,
        }
    }

    /// Success result with plain text content.
    pub fn success_text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::Text { text: text.into() }],
            is_error: false,
        }
    }

    /// Error result with a human-readable message.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![ContentItem::Text {
                text: message.into(),
            }],
            is_error: true,
        }
    }
}
