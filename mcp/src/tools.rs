//! MCP tool implementations: search, get_document, list_stores.
//!
//! Each tool validates its arguments against the declared schema, calls into
//! `core` search/store APIs, and returns structured `CallToolResult` values.
//!
//! See specs/05-surfaces.md §4 and specs/02-domain-model.md §6.

use std::sync::Arc;

use serde_json::Value;

use localdb_core::{
    citation::Citation,
    search::{QueryRequest, QueryResponse, SearchOrchestrator, StoreHandle},
    store::{RetrievalStore, StoreStats},
    Embedder,
};

use crate::protocol::CallToolResult;

// ---------------------------------------------------------------------------
// Typed error helper
// ---------------------------------------------------------------------------

/// Build a structured `CallToolResult` error with machine-readable code and message.
///
/// Content shape: `{"error": {"code": "...", "message": "..."}}`.
/// Use `localdb_core::Error::code()` for the code when mapping a domain error.
fn typed_error(code: &str, message: impl Into<String>) -> CallToolResult {
    let v = serde_json::json!({
        "error": {
            "code": code,
            "message": message.into(),
        }
    });
    CallToolResult {
        content: vec![crate::protocol::ContentItem::Text {
            text: serde_json::to_string_pretty(&v).unwrap_or_default(),
        }],
        is_error: true,
    }
}

// ---------------------------------------------------------------------------
// Store descriptor — a named store with its stats and handle.
// ---------------------------------------------------------------------------

/// Metadata about a store exposed to MCP callers.
#[derive(Debug, Clone)]
pub struct StoreDescriptor {
    /// Store ID (ULID).
    pub id: String,
    /// Store name.
    pub name: String,
    /// Visibility ("private" | "shared").
    pub visibility: String,
}

/// A named store available in this MCP session.
///
/// The store is held behind an `Arc` so it can be cheaply shared
/// with `StoreHandle` without lifetime constraints.
pub struct AvailableStore {
    pub descriptor: StoreDescriptor,
    pub store: Arc<dyn RetrievalStore>,
}

impl AvailableStore {
    /// Create an `AvailableStore` from a boxed store.
    pub fn new(descriptor: StoreDescriptor, store: Box<dyn RetrievalStore>) -> Self {
        Self {
            descriptor,
            store: Arc::from(store),
        }
    }

    /// Create an `AvailableStore` from an `Arc` store.
    pub fn from_arc(descriptor: StoreDescriptor, store: Arc<dyn RetrievalStore>) -> Self {
        Self { descriptor, store }
    }
}

// ---------------------------------------------------------------------------
// Tool: list_stores
// ---------------------------------------------------------------------------

/// Execute the `list_stores` tool.
///
/// Returns names, visibility, and chunk/document counts for every store.
/// No arguments required.
pub async fn tool_list_stores(stores: &[AvailableStore]) -> CallToolResult {
    let mut result = Vec::new();

    for s in stores {
        let stats: StoreStats = match s.store.stats().await {
            Ok(st) => st,
            Err(e) => {
                return typed_error(
                    e.code(),
                    format!(
                        "Failed to get stats for store '{}': {}",
                        s.descriptor.name, e
                    ),
                );
            }
        };

        result.push(serde_json::json!({
            "id": s.descriptor.id,
            "name": s.descriptor.name,
            "visibility": s.descriptor.visibility,
            "chunk_count": stats.chunk_count,
            "document_count": stats.document_count,
        }));
    }

    let v = serde_json::json!({ "stores": result });
    CallToolResult::success_json(&v)
}

// ---------------------------------------------------------------------------
// Tool: search
// ---------------------------------------------------------------------------

/// Arguments for the `search` tool.
#[derive(Debug)]
pub struct SearchArgs {
    /// The natural language query.
    pub query: String,
    /// Optional: restrict to these store names.
    pub store_names: Vec<String>,
    /// Maximum results to return.
    pub limit: usize,
    /// Max characters of snippet text per result in the text rendering.
    pub content_length: usize,
}

impl SearchArgs {
    const DEFAULT_LIMIT: usize = 10;
    const MAX_LIMIT: usize = 100;
    const DEFAULT_CONTENT_LENGTH: usize = 400;

    /// Parse from raw JSON params (the outer `params` object from JSON-RPC,
    /// which contains `name` and `arguments` fields for `tools/call`).
    pub fn from_value(params: Option<&Value>) -> Result<Self, String> {
        let args = params
            .and_then(|p| p.get("arguments"))
            .unwrap_or(&Value::Null);

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing required argument: query".to_string())?
            .to_string();

        if query.trim().is_empty() {
            return Err("query must not be empty".to_string());
        }

        let store_names = args
            .get("stores")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).min(Self::MAX_LIMIT))
            .unwrap_or(Self::DEFAULT_LIMIT);

        let content_length = args
            .get("content_length")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(Self::DEFAULT_CONTENT_LENGTH);

        Ok(SearchArgs {
            query,
            store_names,
            limit,
            content_length,
        })
    }
}

/// Execute the `search` tool.
///
/// Returns a list of citations in the canonical JSON shape
/// (specs/02-domain-model.md §6).
///
/// If `store_names` is non-empty, only those stores are queried.
/// Unknown store name → returns a tool error with code `store_not_found`.
fn select_mcp_stores(
    stores: &[AvailableStore],
    args: &SearchArgs,
) -> Result<Vec<StoreHandle>, CallToolResult> {
    let selected_arcs: Vec<(String, String, Arc<dyn RetrievalStore>)> =
        if args.store_names.is_empty() {
            stores
                .iter()
                .map(|s| {
                    (
                        s.descriptor.id.clone(),
                        s.descriptor.name.clone(),
                        Arc::clone(&s.store),
                    )
                })
                .collect()
        } else {
            let mut selected = Vec::new();
            for name in &args.store_names {
                match stores.iter().find(|s| &s.descriptor.name == name) {
                    Some(s) => selected.push((
                        s.descriptor.id.clone(),
                        s.descriptor.name.clone(),
                        Arc::clone(&s.store),
                    )),
                    None => {
                        return Err(typed_error(
                            "store_not_found",
                            format!("no store named '{name}'"),
                        ));
                    }
                }
            }
            selected
        };

    Ok(selected_arcs
        .into_iter()
        .map(|(id, name, arc)| StoreHandle {
            id,
            name,
            store: arc,
        })
        .collect())
}

fn search_to_tool_result(response: QueryResponse, content_length: usize) -> CallToolResult {
    let citations_json: Vec<Value> = response
        .citations
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();

    let v = serde_json::json!({
        "citations": citations_json,
        "total_candidates": response.total_candidates,
    });

    let text_rendering = render_citations_text(&response.citations, content_length);
    let json_str = serde_json::to_string_pretty(&v).unwrap_or_default();
    let full_text = format!("{json_str}\n\n---\n{text_rendering}");

    CallToolResult {
        content: vec![crate::protocol::ContentItem::Text { text: full_text }],
        is_error: false,
    }
}

pub async fn tool_search(
    stores: &[AvailableStore],
    embedder: &dyn Embedder,
    params: Option<&Value>,
) -> CallToolResult {
    let args = match SearchArgs::from_value(params) {
        Ok(a) => a,
        Err(msg) => return typed_error("invalid_request", format!("invalid arguments: {msg}")),
    };
    if args.limit == 0 {
        return typed_error("invalid_request", "limit must be at least 1");
    }
    let store_handles = match select_mcp_stores(stores, &args) {
        Ok(handles) => handles,
        Err(result) => return result,
    };
    if store_handles.is_empty() {
        return CallToolResult::success_json(&serde_json::json!({ "citations": [] }));
    }
    let request = QueryRequest {
        query: args.query.clone(),
        leg_k: None,
        top_n: Some(args.limit),
        filters: vec![],
    };
    let response = match SearchOrchestrator::query(&store_handles, embedder, &request).await {
        Ok(r) => r,
        Err(e) => return typed_error(e.code(), format!("search failed: {e}")),
    };
    search_to_tool_result(response, args.content_length)
}

/// Render citations as human-readable text for non-structured clients.
pub fn render_citations_text(citations: &[Citation], max_chars: usize) -> String {
    if citations.is_empty() {
        return "No results found.".to_string();
    }

    citations
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let heading = if c.heading_path.is_empty() {
                String::new()
            } else {
                format!(" > {}", c.heading_path.join(" > "))
            };
            let title = c.title.as_deref().unwrap_or("");
            let creator_date = {
                let creator = c.metadata.creator.first().map(|s| s.as_str()).unwrap_or("");
                let date = c.metadata.date.as_deref().unwrap_or("");
                match (creator, date) {
                    ("", "") => String::new(),
                    (cr, "") => format!("\n   {cr}"),
                    ("", dt) => format!("\n   {dt}"),
                    (cr, dt) => format!("\n   {cr} · {dt}"),
                }
            };
            format!(
                "{}. {}{}{}{}\n   Score: {:.4}\n   {}\n",
                i + 1,
                title,
                c.uri,
                heading,
                creator_date,
                c.score.fused,
                c.snippet.chars().take(max_chars).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Tool: get_document
// ---------------------------------------------------------------------------

/// Execute the `get_document` tool.
///
/// Looks up a document by ID across the available stores and returns
/// normalized text + metadata.
///
/// Returns `document_not_found` error if no matching chunks are found.
///
/// Note: URI-based lookup is not supported in v1 (the `RetrievalStore` trait
/// provides `get_chunks_for_document` by ID only).  Callers must use a
/// document ID obtained from a prior `search` call.
pub async fn tool_get_document(
    stores: &[AvailableStore],
    params: Option<&Value>,
) -> CallToolResult {
    let args = match GetDocumentArgs::from_value(params) {
        Ok(args) => args,
        Err(result) => return result,
    };
    match find_document_chunks(stores, &args.id).await {
        Ok(Some((store, chunks))) => CallToolResult::success_json(&document_json(store, &chunks)),
        Ok(None) => typed_error(
            "document_not_found",
            format!("no document with id '{}' found in any store", args.id),
        ),
        Err(result) => result,
    }
}

#[derive(Debug, Clone)]
struct GetDocumentArgs {
    id: String,
}

impl GetDocumentArgs {
    fn from_value(params: Option<&Value>) -> Result<Self, CallToolResult> {
        let args = params
            .and_then(|p| p.get("arguments"))
            .unwrap_or(&Value::Null);
        // Accept "id" (document_id) preferred; "uri" is acknowledged but not supported in v1.
        let doc_id = args.get("id").and_then(|v| v.as_str());
        let uri_arg = args.get("uri").and_then(|v| v.as_str());
        match (doc_id, uri_arg) {
            (None, None) => Err(typed_error(
                "invalid_request",
                "invalid arguments: must provide 'id' (document_id) or 'uri'",
            )),
            (None, Some(_uri)) => Err(typed_error(
                "invalid_request",
                "uri-based get_document is not supported in v1; use the document 'id' from a search result",
            )),
            (Some(id), _) if id.trim().is_empty() => Err(typed_error(
                "invalid_request",
                "invalid arguments: 'id' must not be empty",
            )),
            (Some(id), _) => Ok(Self { id: id.to_string() }),
        }
    }
}

async fn find_document_chunks<'a>(
    stores: &'a [AvailableStore],
    doc_id: &str,
) -> Result<Option<(&'a AvailableStore, Vec<localdb_core::ChunkRecord>)>, CallToolResult> {
    for store in stores {
        let chunks = match store.store.get_chunks_for_document(doc_id).await {
            Ok(chunks) => chunks,
            Err(e) => {
                return Err(typed_error(
                    e.code(),
                    format!(
                        "error fetching document from store '{}': {e}",
                        store.descriptor.name
                    ),
                ));
            }
        };
        if chunks.is_empty() {
            continue;
        }
        let first = &chunks[0];
        if first.store_id != store.descriptor.id {
            continue;
        }
        return Ok(Some((store, chunks)));
    }
    Ok(None)
}

fn document_json(store: &AvailableStore, chunks: &[localdb_core::ChunkRecord]) -> Value {
    let first = &chunks[0];
    let full_text = chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::json!({
        "document_id": first.document_id,
        "uri": first.uri,
        "title": first.metadata.title,
        "store": {
            "id": store.descriptor.id,
            "name": store.descriptor.name,
        },
        "provenance": {
            "fetched_at": first.fetched_at,
            "content_hash": first.content_hash,
        },
        "metadata": first.metadata,
        "chunk_count": chunks.len(),
        "text": full_text,
    })
}

#[cfg(test)]
mod get_document_tests {
    use std::sync::Arc;

    use super::*;
    use localdb_core::ids::{chunk_id, content_hash, document_id, new_ulid};
    use localdb_core::parser::DocumentMetadata;
    use localdb_core::store::{FakeStore, RetrievalStore};
    use localdb_core::{ChunkRecord, Span};

    #[tokio::test]
    async fn tool_get_document_returns_identical_json_for_fixed_document() {
        let store_id = new_ulid();
        let origin_store = new_ulid();
        let source_id = new_ulid();
        let doc_uri = "file:///docs/guide.md";
        let doc_hash = content_hash("guide body");
        let doc_id = document_id(doc_uri, &doc_hash);
        let metadata = DocumentMetadata {
            title: Some("Guide".to_string()),
            creator: vec!["Ada".to_string()],
            subject: vec!["docs".to_string()],
            description: Some("reference document".to_string()),
            publisher: Some("localdb".to_string()),
            contributor: vec!["Bea".to_string()],
            date: Some("2026-06-29".to_string()),
            format: Some("text/markdown".to_string()),
            identifier: Some("guide-1".to_string()),
            language: Some("en".to_string()),
            rights: Some("CC0".to_string()),
            ..Default::default()
        };

        let store = FakeStore::new();
        let make_chunk = |text: &str| {
            let span = Span::new(0, text.len());
            ChunkRecord {
                id: chunk_id(&doc_id, text, span.start, span.end, 0),
                document_id: doc_id.clone(),
                store_id: store_id.clone(),
                text: text.to_string(),
                span,
                heading_path: vec!["Guide".to_string()],
                embedding: vec![0.1, 0.2],
                policy_version: "policy-v1".to_string(),
                fetched_at: "2026-06-29T00:00:00Z".to_string(),
                content_hash: doc_hash.clone(),
                origin_store: origin_store.clone(),
                source_id: source_id.clone(),
                source_kind: "path".to_string(),
                mime: None,
                uri: doc_uri.to_string(),
                metadata: metadata.clone(),
                block_seq: 0,
                seq_in_block: 0,
                block_kind: None,
            }
        };
        store
            .upsert_chunks(vec![make_chunk("alpha"), make_chunk("beta")])
            .await
            .unwrap();

        let stores = vec![AvailableStore::from_arc(
            StoreDescriptor {
                id: store_id.to_string(),
                name: "notes".to_string(),
                visibility: "private".to_string(),
            },
            Arc::new(store),
        )];
        let params = serde_json::json!({"arguments": {"id": doc_id}});

        let result = tool_get_document(&stores, Some(&params)).await;
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);

        let rendered_text = match &result.content[0] {
            crate::protocol::ContentItem::Text { text } => text,
        };

        let expected = serde_json::json!({
            "document_id": doc_id,
            "uri": doc_uri,
            "title": "Guide",
            "store": {
                "id": store_id.to_string(),
                "name": "notes",
            },
            "provenance": {
                "fetched_at": "2026-06-29T00:00:00Z",
                "content_hash": doc_hash,
            },
            "metadata": metadata,
            "chunk_count": 2,
            "text": "alpha\nbeta",
        });
        let expected = serde_json::to_string_pretty(&expected).unwrap();

        assert_eq!(rendered_text, &expected);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::{embedder::FakeEmbedder, store::FakeStore, types::Span, ChunkRecord};

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_descriptor(id: &str, name: &str) -> StoreDescriptor {
        StoreDescriptor {
            id: id.to_string(),
            name: name.to_string(),
            visibility: "private".to_string(),
        }
    }

    fn make_chunk(id: &str, document_id: &str, store_id: &str, text: &str) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            document_id: document_id.to_string(),
            store_id: store_id.to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding: vec![0.0; 128],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-12T00:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            source_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: format!("file:///docs/{document_id}.md"),
            metadata: localdb_core::parser::DocumentMetadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
        }
    }

    fn build_params(json: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "arguments": json })
    }

    // -----------------------------------------------------------------------
    // E4 — search rejects limit=0
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn search_tool_rejects_limit_zero() {
        let store = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "mystore"), Box::new(store));
        let embedder = FakeEmbedder::new(128);
        // Pass limit: 0 explicitly.  SearchArgs::from_value clamps by MAX but we bypass
        // that by injecting limit=0 directly via a custom from_value call.
        // To test the tool-level guard we must call from_value with limit=0.
        // The JSON schema declares minimum:1, but we test the runtime guard here.
        let params = serde_json::json!({
            "arguments": {
                "query": "hello",
                "limit": 0
            }
        });
        let result = tool_search(&[av], &embedder, Some(&params)).await;
        assert!(result.is_error, "limit=0 should produce an error result");
        let text = match result.content.first().unwrap() {
            crate::protocol::ContentItem::Text { text } => text.clone(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("error body is JSON");
        assert_eq!(
            parsed["error"]["code"].as_str().unwrap(),
            "invalid_request",
            "error code should be invalid_request"
        );
        assert!(
            parsed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("limit must be at least 1"),
            "error message should mention limit"
        );
    }

    // Confirm that limit=0 coming through SearchArgs::from_value is caught.
    // SearchArgs clamps to MAX_LIMIT so we test from_value independently too.
    #[test]
    fn search_args_limit_zero_passes_through() {
        // from_value does not reject limit=0 itself (that's the tool's job).
        // Verify it produces limit=0 so the tool guard can fire.
        let params = serde_json::json!({
            "arguments": {
                "query": "test",
                "limit": 0
            }
        });
        let args = SearchArgs::from_value(Some(&params)).expect("from_value should succeed");
        // 0_usize.min(MAX_LIMIT) == 0, so limit should be 0
        assert_eq!(args.limit, 0, "limit=0 should survive from_value unchanged");
    }

    // -----------------------------------------------------------------------
    // E3 — get_document checks store scope visibility
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_document_returns_not_found_when_store_id_mismatches() {
        // Set up a store whose descriptor id is "store-A" but the chunk's store_id
        // is "store-B" (simulating a federated/mismatched scenario).
        let fake = FakeStore::new();
        // Insert a chunk that claims to belong to "store-B", not "store-A".
        let chunk = make_chunk("chunk-1", "doc-mismatched", "store-B", "some content");
        fake.upsert_chunks(vec![chunk]).await.unwrap();

        // The AvailableStore has descriptor id "store-A" — the chunk's store_id doesn't match.
        let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

        let params = build_params(serde_json::json!({ "id": "doc-mismatched" }));
        let result = tool_get_document(&[av], Some(&params)).await;

        // The tool should hide the document (not leak existence) and return not-found.
        assert!(
            result.is_error,
            "mismatched store_id should cause document_not_found"
        );
        let text = match result.content.first().unwrap() {
            crate::protocol::ContentItem::Text { text } => text.clone(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("error body is JSON");
        assert_eq!(
            parsed["error"]["code"].as_str().unwrap(),
            "document_not_found",
        );
    }

    #[tokio::test]
    async fn get_document_succeeds_when_store_id_matches() {
        let fake = FakeStore::new();
        let chunk = make_chunk("chunk-1", "doc-1", "store-A", "hello world");
        fake.upsert_chunks(vec![chunk]).await.unwrap();

        let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

        let params = build_params(serde_json::json!({ "id": "doc-1" }));
        let result = tool_get_document(&[av], Some(&params)).await;

        assert!(!result.is_error, "matching store_id should succeed");
        let text = match result.content.first().unwrap() {
            crate::protocol::ContentItem::Text { text } => text.clone(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("success body is JSON");
        assert_eq!(parsed["document_id"].as_str().unwrap(), "doc-1");
        assert!(
            parsed.get("metadata").is_some(),
            "metadata field must be present"
        );
    }

    #[tokio::test]
    async fn get_document_metadata_carries_through() {
        let fake = FakeStore::new();
        let mut chunk = make_chunk("chunk-1", "doc-meta", "store-A", "text content");
        chunk.metadata = localdb_core::parser::DocumentMetadata {
            title: Some("Rich Doc".to_string()),
            creator: vec!["Carol".to_string()],
            date: Some("2026-05-01".to_string()),
            ..Default::default()
        };
        fake.upsert_chunks(vec![chunk]).await.unwrap();

        let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

        let params = build_params(serde_json::json!({ "id": "doc-meta" }));
        let result = tool_get_document(&[av], Some(&params)).await;

        assert!(!result.is_error);
        let text = match result.content.first().unwrap() {
            crate::protocol::ContentItem::Text { text } => text.clone(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        let meta = &parsed["metadata"];
        assert_eq!(meta["title"].as_str().unwrap(), "Rich Doc");
        assert_eq!(
            meta["creator"].as_array().unwrap()[0].as_str().unwrap(),
            "Carol"
        );
        assert_eq!(meta["date"].as_str().unwrap(), "2026-05-01");
    }

    // -----------------------------------------------------------------------
    // E2 — typed error shape
    // -----------------------------------------------------------------------

    #[test]
    fn typed_error_helper_produces_correct_shape() {
        let result = typed_error("store_not_found", "no store named 'foo'");
        assert!(result.is_error);
        let text = match result.content.first().unwrap() {
            crate::protocol::ContentItem::Text { text } => text.clone(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
        assert!(parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no store named 'foo'"));
    }

    #[tokio::test]
    async fn search_returns_empty_citations_not_error_when_no_results() {
        // E2 also requires: 0 results → {"citations": []} not an error.
        let fake = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));
        let embedder = FakeEmbedder::new(128);

        let params = serde_json::json!({
            "arguments": { "query": "totally absent term xyzzy" }
        });
        let result = tool_search(&[av], &embedder, Some(&params)).await;
        // Should NOT be an error — just empty citations.
        assert!(!result.is_error, "empty results should not be an error");
    }

    #[tokio::test]
    async fn get_document_missing_id_returns_typed_error() {
        let fake = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));

        // No 'id' or 'uri' argument.
        let params = build_params(serde_json::json!({}));
        let result = tool_get_document(&[av], Some(&params)).await;
        assert!(result.is_error);
        let text = match result.content.first().unwrap() {
            crate::protocol::ContentItem::Text { text } => text.clone(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
    }

    #[tokio::test]
    async fn search_unknown_store_returns_typed_error() {
        let fake = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "real-store"), Box::new(fake));
        let embedder = FakeEmbedder::new(128);

        let params = serde_json::json!({
            "arguments": {
                "query": "hello",
                "stores": ["nonexistent-store"]
            }
        });
        let result = tool_search(&[av], &embedder, Some(&params)).await;
        assert!(result.is_error);
        let text = match result.content.first().unwrap() {
            crate::protocol::ContentItem::Text { text } => text.clone(),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
    }

    // -----------------------------------------------------------------------
    // render_citations_text — creator · date formatting
    // -----------------------------------------------------------------------

    fn make_citation_with_metadata(
        uri: &str,
        creator: Vec<String>,
        date: Option<String>,
    ) -> localdb_core::citation::Citation {
        use localdb_core::{
            citation::{CitationProvenance, CitationStore, Score},
            parser::DocumentMetadata,
            types::Span,
        };
        localdb_core::citation::Citation {
            chunk_id: "c1".to_string(),
            document_id: "d1".to_string(),
            store: CitationStore {
                id: "s1".to_string(),
                name: "store".to_string(),
            },
            uri: uri.to_string(),
            title: None,
            heading_path: vec![],
            span: Span::new(0, 4),
            snippet: "text".to_string(),
            score: Score {
                fused: 0.5,
                dense: None,
                bm25: None,
            },
            provenance: CitationProvenance {
                fetched_at: "2026-01-01T00:00:00Z".to_string(),
                content_hash: "abc".to_string(),
            },
            metadata: DocumentMetadata {
                creator,
                date,
                ..Default::default()
            },
            block_seq: None,
            block_kind: None,
        }
    }

    #[test]
    fn render_citations_text_shows_creator_and_date() {
        let c = make_citation_with_metadata(
            "file:///a.md",
            vec!["Alice".to_string()],
            Some("2026-03-01".to_string()),
        );
        let text = render_citations_text(&[c], 400);
        assert!(
            text.contains("Alice · 2026-03-01"),
            "should show creator · date, got: {text}"
        );
    }

    #[test]
    fn render_citations_text_date_only() {
        let c = make_citation_with_metadata("file:///a.md", vec![], Some("2026-03-01".to_string()));
        let text = render_citations_text(&[c], 400);
        assert!(text.contains("2026-03-01"), "should show date, got: {text}");
        assert!(!text.contains('·'), "should not show · with no creator");
    }

    #[test]
    fn render_citations_text_creator_only() {
        let c = make_citation_with_metadata("file:///a.md", vec!["Bob".to_string()], None);
        let text = render_citations_text(&[c], 400);
        assert!(text.contains("Bob"), "should show creator, got: {text}");
        assert!(!text.contains('·'), "should not show · with no date");
    }

    #[test]
    fn render_citations_text_no_metadata() {
        let c = make_citation_with_metadata("file:///a.md", vec![], None);
        let text = render_citations_text(&[c], 400);
        assert!(!text.contains('·'), "no metadata — no · separator");
    }

    #[test]
    fn render_citations_text_respects_custom_content_length() {
        let mut c = make_citation_with_metadata("file:///a.md", vec![], None);
        c.snippet = "word ".repeat(200);
        let text = render_citations_text(&[c], 50);
        let snippet_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("word"))
            .unwrap();
        assert!(
            snippet_line.trim().chars().count() <= 50,
            "snippet should be capped at 50 chars, got: {snippet_line}"
        );
    }

    #[test]
    fn search_args_default_content_length() {
        let params = serde_json::json!({
            "name": "search",
            "arguments": { "query": "hello" }
        });
        let args = SearchArgs::from_value(Some(&params)).unwrap();
        assert_eq!(
            args.content_length, 400,
            "default content_length should be 400"
        );
    }

    #[test]
    fn search_args_custom_content_length() {
        let params = serde_json::json!({
            "name": "search",
            "arguments": { "query": "hello", "content_length": 50 }
        });
        let args = SearchArgs::from_value(Some(&params)).unwrap();
        assert_eq!(args.content_length, 50);
    }
}
