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
    error::Error,
    search::{QueryRequest, QueryResponse, SearchOrchestrator, StoreHandle},
    store::{ChunkRecord, MetadataFilter, RetrievalStore, SearchResult, StoreStats},
    Embedder,
};

use crate::protocol::CallToolResult;

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
                return CallToolResult::error(format!(
                    "Failed to get stats for store '{}': {}",
                    s.descriptor.name, e
                ));
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
}

impl SearchArgs {
    const DEFAULT_LIMIT: usize = 10;
    const MAX_LIMIT: usize = 100;

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

        Ok(SearchArgs {
            query,
            store_names,
            limit,
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
pub async fn tool_search(
    stores: &[AvailableStore],
    embedder: &dyn Embedder,
    params: Option<&Value>,
) -> CallToolResult {
    let args = match SearchArgs::from_value(params) {
        Ok(a) => a,
        Err(msg) => return CallToolResult::error(format!("invalid arguments: {msg}")),
    };

    // Filter stores by requested names (if any).
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
                        return CallToolResult::error(format!(
                            "store_not_found: no store named '{name}'"
                        ));
                    }
                }
            }
            selected
        };

    if selected_arcs.is_empty() {
        let v = serde_json::json!({ "citations": [] });
        return CallToolResult::success_json(&v);
    }

    // Build StoreHandle list for the orchestrator.
    // ArcStore wraps an Arc so it satisfies Box<dyn RetrievalStore> (RetrievalStore: 'static).
    let store_handles: Vec<StoreHandle> = selected_arcs
        .into_iter()
        .map(|(id, name, arc)| StoreHandle {
            id,
            name,
            store: Box::new(ArcStore(arc)),
        })
        .collect();

    let request = QueryRequest {
        query: args.query.clone(),
        leg_k: None,
        top_n: Some(args.limit),
        filters: vec![],
    };

    let response: QueryResponse =
        match SearchOrchestrator::query(&store_handles, embedder, &request).await {
            Ok(r) => r,
            Err(e) => return CallToolResult::error(format!("search failed: {e}")),
        };

    let citations_json: Vec<Value> = response
        .citations
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();

    let v = serde_json::json!({
        "citations": citations_json,
        "total_candidates": response.total_candidates,
    });

    // Also build a short text rendering for non-structured clients.
    let text_rendering = render_citations_text(&response.citations);

    // Return both structured JSON and text rendering in the same text content item.
    let json_str = serde_json::to_string_pretty(&v).unwrap_or_default();
    let full_text = format!("{json_str}\n\n---\n{text_rendering}");

    CallToolResult {
        content: vec![crate::protocol::ContentItem::Text { text: full_text }],
        is_error: false,
    }
}

/// Render citations as human-readable text for non-structured clients.
pub fn render_citations_text(citations: &[Citation]) -> String {
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
            format!(
                "{}. {}{}{}\n   Score: {:.4}\n   {}\n",
                i + 1,
                title,
                c.uri,
                heading,
                c.score.fused,
                c.snippet.chars().take(200).collect::<String>()
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
    let args = params
        .and_then(|p| p.get("arguments"))
        .unwrap_or(&Value::Null);

    // Accept "id" (document_id) preferred; "uri" is acknowledged but not supported in v1.
    let doc_id = args.get("id").and_then(|v| v.as_str());
    let uri_arg = args.get("uri").and_then(|v| v.as_str());

    match (doc_id, uri_arg) {
        (None, None) => {
            return CallToolResult::error(
                "invalid arguments: must provide 'id' (document_id) or 'uri'",
            );
        }
        (None, Some(_uri)) => {
            // URI-based lookup: not supported in v1 (no index on URI).
            return CallToolResult::error(
                "uri-based get_document is not supported in v1; use the document 'id' from a search result",
            );
        }
        (Some(id), _) if id.trim().is_empty() => {
            return CallToolResult::error("invalid arguments: 'id' must not be empty");
        }
        _ => {}
    }

    let doc_id = doc_id.unwrap();

    // Search all stores for matching chunks by document ID.
    for s in stores {
        let chunks = match s.store.get_chunks_for_document(doc_id).await {
            Ok(c) => c,
            Err(e) => {
                return CallToolResult::error(format!(
                    "error fetching document from store '{}': {e}",
                    s.descriptor.name
                ));
            }
        };

        if !chunks.is_empty() {
            // Build document metadata from the first chunk.
            let first = &chunks[0];
            let full_text = chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            let v = serde_json::json!({
                "document_id": first.document_id,
                "uri": first.uri,
                "title": first.title,
                "store": {
                    "id": s.descriptor.id,
                    "name": s.descriptor.name,
                },
                "provenance": {
                    "fetched_at": first.fetched_at,
                    "content_hash": first.content_hash,
                },
                "chunk_count": chunks.len(),
                "text": full_text,
            });

            return CallToolResult::success_json(&v);
        }
    }

    CallToolResult::error(format!(
        "document_not_found: no document with id '{doc_id}' found in any store"
    ))
}

// ---------------------------------------------------------------------------
// ArcStore wrapper — allows using Arc<dyn RetrievalStore> as Box<dyn RetrievalStore>
// ---------------------------------------------------------------------------

/// Wraps an `Arc<dyn RetrievalStore>` so it can be placed in a `Box<dyn RetrievalStore>`.
///
/// `Box<dyn RetrievalStore>` requires `'static`, but `Arc` satisfies that constraint.
struct ArcStore(Arc<dyn RetrievalStore>);

#[async_trait::async_trait]
impl RetrievalStore for ArcStore {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        self.0.upsert_chunks(records).await
    }

    async fn delete_by_document(&self, document_id: &str) -> Result<usize, Error> {
        self.0.delete_by_document(document_id).await
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        self.0.delete_by_store(store_id).await
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        self.0.dense_search(query_vector, limit, filters).await
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        self.0.bm25_search(query_text, limit, filters).await
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        self.0.stats().await
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        self.0.get_chunk(chunk_id).await
    }

    async fn get_chunks_for_document(&self, document_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        self.0.get_chunks_for_document(document_id).await
    }
}
