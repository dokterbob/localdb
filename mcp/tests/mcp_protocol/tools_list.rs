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

/// The new filter properties (issue #247) must appear in the generated
/// `search` tool schema, each with its own description — the only guard
/// between `docs/mcp.md`'s hand-copied schema snippet and silent drift.
#[tokio::test]
async fn test_search_schema_includes_filter_properties_with_descriptions() {
    let client = client_for(make_handler_with_one_store()).await;
    let result = client.list_tools(None).await.expect("list_tools succeeds");

    let search_tool = result
        .tools
        .iter()
        .find(|t| t.name.as_ref() == "search")
        .expect("search tool must be present");

    let properties = search_tool
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("search tool inputSchema must have an object 'properties'");

    for field in [
        "path",
        "mime",
        "added_after",
        "added_before",
        "updated_after",
        "updated_before",
        "modified_after",
        "modified_before",
        "document_after",
        "document_before",
    ] {
        let prop = properties
            .get(field)
            .unwrap_or_else(|| panic!("search tool schema is missing filter property '{field}'"));
        let description = prop
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("filter property '{field}' must have a description"));
        assert!(
            !description.is_empty(),
            "filter property '{field}' description must not be empty"
        );
    }
}
