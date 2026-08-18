//! `tools/list` tests: the exact tool set, required fields, and that no
//! mutating tool is reachable.

use serde_json::Value;

use crate::harness::{client_for, make_handler_with_one_store};

/// T03: tools/list returns exactly the five read-only tools
#[tokio::test]
async fn test_tools_list_exact_five_tools() {
    let client = client_for(make_handler_with_one_store()).await;

    let result = client.list_tools(None).await.expect("list_tools succeeds");
    assert_eq!(result.tools.len(), 5, "should expose exactly 5 tools");

    let tool_names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(tool_names.contains(&"search"), "should have 'search' tool");
    assert!(
        tool_names.contains(&"get_document"),
        "should have 'get_document' tool"
    );
    assert!(
        tool_names.contains(&"get_chunks"),
        "should have 'get_chunks' tool"
    );
    assert!(
        tool_names.contains(&"list_stores"),
        "should have 'list_stores' tool"
    );
    assert!(
        tool_names.contains(&"list_documents"),
        "should have 'list_documents' tool"
    );
}

/// T04: each tool has a name, description, and inputSchema
#[tokio::test]
async fn test_tools_have_required_fields() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = client.list_tools(None).await.expect("list_tools succeeds");

    for tool in &result.tools {
        assert!(!tool.name.is_empty(), "tool name must not be empty");
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "tool '{}' must have a non-empty description",
            tool.name
        );
        assert_eq!(
            tool.input_schema.get("type").and_then(Value::as_str),
            Some("object"),
            "tool '{}' inputSchema must be a JSON Schema object",
            tool.name
        );
    }
}

/// T17: no mutating tool is accessible (only the 5 read-only tools exist)
#[tokio::test]
async fn test_no_mutating_tools_accessible() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = client.list_tools(None).await.expect("list_tools succeeds");
    let tool_names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();

    let mutating = [
        "add_source",
        "remove_source",
        "reindex",
        "delete_document",
        "upsert_chunk",
        "create_store",
        "delete_store",
    ];
    for m in mutating {
        assert!(
            !tool_names.contains(&m),
            "mutating tool '{m}' must not be accessible"
        );
    }
}
