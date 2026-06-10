//! MCP server for localdb.
//!
//! Stdio MCP server exposing read-only tools:
//! - `search`: hybrid search returning Citation list
//! - `get_document`: fetch normalized text + metadata by id or uri
//! - `list_stores`: names, visibility, counts
//!
//! Implemented in T10.
