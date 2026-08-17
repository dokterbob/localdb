//! Unknown-tool-name dispatch test — the one case that surfaces as a
//! protocol-level error rather than a tool-level one (see `main.rs`'s
//! module doc for the full two-tier model).

use serde_json::json;

use rmcp::{model::ErrorCode, service::ServiceError};

use crate::harness::{call_tool, client_for, make_handler_with_one_store};

/// T18 (changed expectation): calling an unregistered tool name is now
/// dispatched by rmcp's own macro-generated `call_tool`, which returns a
/// protocol-level error rather than the old hand-written tool-level
/// `CallToolResult::error("unknown tool '...'")`. Confirmed against rmcp
/// 1.8.0 source (`handler/server/router/tool.rs`): unmatched names return
/// `ErrorData::invalid_params("tool not found", None)`.
#[tokio::test]
async fn test_unknown_tool_call() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = call_tool(&client, "add_source", json!({ "path": "/evil" })).await;

    match result {
        Err(ServiceError::McpError(e)) => {
            assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(e.message, "tool not found");
        }
        other => panic!("expected a protocol-level McpError, got {other:?}"),
    }
}
