//! MCP server for localdb.
//!
//! Stdio MCP server exposing read-only tools:
//! - `search`: hybrid search returning Citation list (canonical JSON shape)
//! - `get_document`: fetch normalized text + metadata by id or uri
//! - `list_stores`: names, visibility, chunk/document counts
//!
//! The server speaks the Model Context Protocol (MCP) v1 over stdio using
//! JSON-RPC 2.0.  Each message is a newline-delimited JSON object.
//!
//! ## Process model
//! The server probes the daemon socket on startup; if a daemon is running it
//! delegates to its HTTP API, otherwise it opens the store in-process (embedded
//! mode).  The `--allow-write` flag is parsed but always rejected in v1.
//!
//! See specs/05-surfaces.md §4 and specs/01-architecture.md §3.

pub mod protocol;
pub mod server;
pub mod tools;

// Re-export key items for the binary entry point.
pub use server::{run_stdio_loop, McpServer};
pub use tools::{AvailableStore, StoreDescriptor};
