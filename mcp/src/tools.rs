//! MCP tool implementations: search, get_document, get_chunks, list_stores.
//!
//! Each tool receives its arguments as an already-typed struct from
//! `args.rs` (rmcp's `Parameters<T>` extractor deserializes `tools/call`
//! JSON into these before a tool method ever runs — see `handler.rs`), does
//! its own semantic/business validation, calls into `core` search/store
//! APIs, and returns a structured `rmcp::model::CallToolResult`.
//!
//! See specs/05-surfaces.md §4 and specs/02-domain-model.md §6.

use std::sync::Arc;

use serde_json::Value;

use rmcp::model::{CallToolResult, Content};

use localdb_core::{
    citation::Citation,
    search::{QueryRequest, QueryResponse, SearchOrchestrator, StoreHandle},
    store::{RetrievalStore, StoreStats},
    Embedder,
};

use crate::args::{GetChunksArgs, GetDocumentArgs, SearchArgs};

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
    CallToolResult::error(vec![Content::text(
        serde_json::to_string_pretty(&v).unwrap_or_default(),
    )])
}

/// Build a successful `CallToolResult` carrying pretty-printed JSON as its
/// single text content item.
fn success_json(value: &Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )])
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
/// with `StoreHandle` without lifetime constraints, and so `AvailableStore`
/// itself is cheap to clone (needed for Phase 2's per-HTTP-session handler
/// construction).
#[derive(Clone)]
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
    success_json(&v)
}

// ---------------------------------------------------------------------------
// Tool: search
// ---------------------------------------------------------------------------

const SEARCH_DEFAULT_LIMIT: usize = 10;
const SEARCH_MAX_LIMIT: usize = 100;
const SEARCH_DEFAULT_CONTENT_LENGTH: usize = 400;

/// Resolve `SearchArgs::limit` to a `usize`, preserving the pre-rmcp
/// behavior: absent -> default; a valid non-negative integer -> clamped to
/// `SEARCH_MAX_LIMIT`; a negative integer -> silently falls back to the
/// default (mirroring the old raw-JSON `Value::as_u64()` parse, which
/// simply failed to match on negative numbers and fell through to
/// `unwrap_or(DEFAULT_LIMIT)`). An explicit `0` passes through unchanged so
/// the tool-level guard in `tool_search` can reject it.
fn resolve_search_limit(limit: Option<i64>) -> usize {
    match limit {
        None => SEARCH_DEFAULT_LIMIT,
        Some(n) => usize::try_from(n)
            .map(|v| v.min(SEARCH_MAX_LIMIT))
            .unwrap_or(SEARCH_DEFAULT_LIMIT),
    }
}

/// Resolve `SearchArgs::content_length` to a `usize`, mirroring the same
/// absent-vs-negative-vs-valid handling as `resolve_search_limit` (no
/// separate max clamp — this is a soft snippet-length cap, not respected as
/// a hard runtime bound beyond `usize`).
fn resolve_content_length(content_length: Option<i64>) -> usize {
    match content_length {
        None => SEARCH_DEFAULT_CONTENT_LENGTH,
        Some(n) => usize::try_from(n).unwrap_or(SEARCH_DEFAULT_CONTENT_LENGTH),
    }
}

/// Execute the `search` tool.
///
/// Returns a list of citations in the canonical JSON shape
/// (specs/02-domain-model.md §6).
///
/// If `stores` is non-empty, only those store names are queried.
/// Unknown store name → returns a tool error with code `store_not_found`.
fn select_mcp_stores(
    stores: &[AvailableStore],
    store_names: &[String],
) -> Result<Vec<StoreHandle>, CallToolResult> {
    let selected_arcs: Vec<(String, String, Arc<dyn RetrievalStore>)> = if store_names.is_empty() {
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
        for name in store_names {
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

    CallToolResult::success(vec![Content::text(full_text)])
}

pub async fn tool_search(
    stores: &[AvailableStore],
    embedder: &dyn Embedder,
    args: SearchArgs,
) -> CallToolResult {
    if args.query.trim().is_empty() {
        return typed_error(
            "invalid_request",
            "invalid arguments: query must not be empty",
        );
    }
    let limit = resolve_search_limit(args.limit);
    let content_length = resolve_content_length(args.content_length);
    if limit == 0 {
        return typed_error("invalid_request", "limit must be at least 1");
    }
    let store_names = args.stores.unwrap_or_default();
    let store_handles = match select_mcp_stores(stores, &store_names) {
        Ok(handles) => handles,
        Err(result) => return result,
    };
    if store_handles.is_empty() {
        return success_json(&serde_json::json!({ "citations": [] }));
    }
    let request = QueryRequest {
        query: args.query.clone(),
        leg_k: None,
        top_n: Some(limit),
        filters: vec![],
    };
    let response = match SearchOrchestrator::query(&store_handles, embedder, &request).await {
        Ok(r) => r,
        Err(e) => return typed_error(e.code(), format!("search failed: {e}")),
    };
    search_to_tool_result(response, content_length)
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
                let dc = c.metadata.dublin_core();
                let creator = dc.creator.first().map(|s| s.as_str()).unwrap_or("");
                let date = dc.date.as_deref().unwrap_or("");
                match (creator, date) {
                    ("", "") => String::new(),
                    (cr, "") => format!("\n   {cr}"),
                    ("", dt) => format!("\n   {dt}"),
                    (cr, dt) => format!("\n   {cr} · {dt}"),
                }
            };
            // `content_length` is a soft cap: snap to a natural boundary
            // rather than hard-cutting mid-word/mid-sentence. Only the text
            // rendering is truncated — the JSON citation payload (`c.snippet`
            // as serialized elsewhere) always carries the full snippet.
            let (body, was_truncated) = localdb_core::truncate_snippet(&c.snippet, max_chars);
            let snippet_text = if was_truncated {
                format!("{body}…")
            } else {
                body.to_string()
            };
            format!(
                "{}. {}{}{}{}\n   Score: {:.4}\n   {}\n",
                i + 1,
                title,
                c.uri,
                heading,
                creator_date,
                c.score.fused,
                snippet_text
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
/// Returns `resource_not_found` error if no matching chunks are found.
///
/// Note: URI-based lookup is not supported in v1 (the `RetrievalStore` trait
/// provides `get_chunks_for_resource` by ID only). Callers must use a
/// document ID obtained from a prior `search` call. `id` is a required
/// field on `GetDocumentArgs`, so a caller omitting it entirely never
/// reaches this function — rmcp's `Parameters<T>` extractor fails first,
/// which is still a tool-level error (see `mcp/src/lib.rs`'s two-tier error
/// model doc), just with a generic rmcp-authored message. An explicit empty
/// string still reaches here and is rejected below (with a more specific
/// message when `uri` was given instead, preserving v1's guidance).
pub async fn tool_get_document(stores: &[AvailableStore], args: GetDocumentArgs) -> CallToolResult {
    if args.id.trim().is_empty() {
        if args.uri.is_some() {
            return typed_error(
                "invalid_request",
                "uri-based get_document is not supported in v1; use the document 'id' from a search result",
            );
        }
        return typed_error(
            "invalid_request",
            "invalid arguments: 'id' must not be empty",
        );
    }
    match find_document_chunks(stores, &args.id).await {
        Ok(Some((store, chunks))) => success_json(&document_json(store, &chunks)),
        Ok(None) => typed_error(
            "resource_not_found",
            format!("no document with id '{}' found in any store", args.id),
        ),
        Err(result) => result,
    }
}

async fn find_document_chunks<'a>(
    stores: &'a [AvailableStore],
    doc_id: &str,
) -> Result<Option<(&'a AvailableStore, Vec<localdb_core::ChunkRecord>)>, CallToolResult> {
    for store in stores {
        let chunks = match store.store.get_chunks_for_resource(doc_id).await {
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
        "resource_id": first.resource_id,
        "uri": first.uri,
        "title": first.metadata.title(),
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

// ---------------------------------------------------------------------------
// Tool: get_chunks
// ---------------------------------------------------------------------------

const GET_CHUNKS_DEFAULT_LIMIT: usize = 50;
const GET_CHUNKS_MAX_LIMIT: usize = 200;

/// Resolve `GetChunksArgs::offset`/`limit` to validated `usize`s.
///
/// Distinguishes absent (→ default) from present-but-invalid (→ error),
/// same as the pre-rmcp raw-JSON parsing: an explicit negative offset/limit,
/// or an explicit `limit: 0`, is a tool-level `invalid_request` error rather
/// than a silent default or clamp (clamping `0` up to `1` would silently
/// return a chunk the caller did not ask for). A valid `limit` is capped at
/// `GET_CHUNKS_MAX_LIMIT`.
fn resolve_get_chunks_pagination(args: &GetChunksArgs) -> Result<(usize, usize), CallToolResult> {
    let offset = match args.offset {
        None => 0,
        Some(n) => usize::try_from(n).map_err(|_| {
            typed_error(
                "invalid_request",
                "invalid arguments: 'offset' must be a non-negative integer",
            )
        })?,
    };

    let limit = match args.limit {
        None => GET_CHUNKS_DEFAULT_LIMIT,
        Some(0) => {
            return Err(typed_error(
                "invalid_request",
                "invalid arguments: 'limit' must be at least 1",
            ));
        }
        Some(n) => usize::try_from(n)
            .map(|v| v.min(GET_CHUNKS_MAX_LIMIT))
            .map_err(|_| {
                typed_error(
                    "invalid_request",
                    "invalid arguments: 'limit' must be a positive integer",
                )
            })?,
    };

    Ok((offset, limit))
}

/// Execute the `get_chunks` tool.
///
/// Looks up a document's chunks across the available stores and returns
/// them in order — sorted by `(block_seq, seq_in_block)` — sliced to the
/// requested `offset`/`limit` page.
///
/// Pagination is applied here in the tool rather than added as a new
/// `RetrievalStore` trait method: documents are chunk-bounded (at most a
/// few hundred chunks), so slicing an already-fetched `Vec` is cheap, and a
/// trait change would ripple into every backend implementation plus the
/// conformance test suite for no measured benefit.
///
/// The store layer (libsql) returns chunks pre-sorted, but this function
/// sorts defensively so the contract — deterministic pagination — holds
/// for any `RetrievalStore` implementation, including `FakeStore`, which
/// does not guarantee ordering. The sort key is
/// `(block_seq, seq_in_block, span.start, span.end, chunk_id)`: the trailing
/// fields break ties among legacy records that share `(block_seq,
/// seq_in_block) = (0, 0)`, and `chunk_id` (content-addressed, unique) makes
/// the order total, so a given `offset`/`limit` returns the same page on
/// every call regardless of backend return order.
///
/// Returns `resource_not_found` error if no matching chunks are found.
/// An out-of-range `offset` yields an empty `chunks` array, not an error.
///
/// Note: URI-based lookup is not supported in v1, matching `get_document`.
pub async fn tool_get_chunks(stores: &[AvailableStore], args: GetChunksArgs) -> CallToolResult {
    if args.resource_id.trim().is_empty() {
        return typed_error(
            "invalid_request",
            "invalid arguments: 'resource_id' must not be empty",
        );
    }
    let (offset, limit) = match resolve_get_chunks_pagination(&args) {
        Ok(v) => v,
        Err(result) => return result,
    };
    match find_document_chunks(stores, &args.resource_id).await {
        Ok(Some((store, mut chunks))) => {
            chunks.sort_by(|a, b| {
                (a.block_seq, a.seq_in_block, a.span.start, a.span.end, &a.id).cmp(&(
                    b.block_seq,
                    b.seq_in_block,
                    b.span.start,
                    b.span.end,
                    &b.id,
                ))
            });
            success_json(&chunks_json(store, &chunks, offset, limit))
        }
        Ok(None) => typed_error(
            "resource_not_found",
            format!(
                "no document with id '{}' found in any store",
                args.resource_id
            ),
        ),
        Err(result) => result,
    }
}

fn chunk_summary_json(chunk: &localdb_core::ChunkRecord) -> Value {
    serde_json::json!({
        "chunk_id": chunk.id,
        "block_seq": chunk.block_seq,
        "seq_in_block": chunk.seq_in_block,
        "block_kind": chunk.block_kind,
        "span": {
            "start": chunk.span.start,
            "end": chunk.span.end,
        },
        "heading_path": chunk.heading_path,
        "text": chunk.text,
    })
}

fn chunks_json(
    store: &AvailableStore,
    sorted_chunks: &[localdb_core::ChunkRecord],
    offset: usize,
    limit: usize,
) -> Value {
    let first = &sorted_chunks[0];
    let total_chunks = sorted_chunks.len();
    let end = offset.saturating_add(limit).min(total_chunks);
    let page: Vec<Value> = if offset >= total_chunks {
        Vec::new()
    } else {
        sorted_chunks[offset..end]
            .iter()
            .map(chunk_summary_json)
            .collect()
    };
    let returned = page.len();

    serde_json::json!({
        "resource_id": first.resource_id,
        "uri": first.uri,
        "title": first.metadata.title(),
        "store": {
            "id": store.descriptor.id,
            "name": store.descriptor.name,
        },
        "total_chunks": total_chunks,
        "offset": offset,
        "limit": limit,
        "returned": returned,
        "chunks": page,
    })
}

#[cfg(test)]
mod get_document_tests {
    use std::sync::Arc;

    use super::*;
    use localdb_core::ids::{chunk_id, content_hash, new_ulid, resource_id};
    use localdb_core::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};
    use localdb_core::store::{FakeStore, RetrievalStore};
    use localdb_core::{ChunkRecord, Span};

    fn text_of(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    #[tokio::test]
    async fn tool_get_document_returns_identical_json_for_fixed_document() {
        let store_id = new_ulid();
        let origin_store = new_ulid();
        let source_id = new_ulid();
        let doc_uri = "file:///docs/guide.md";
        let doc_hash = content_hash("guide body");
        let doc_id = resource_id(doc_uri, &doc_hash);
        let metadata = Metadata::Document(DocumentMetadata {
            dublin_core: DublinCoreMetadata {
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
            },
            ..Default::default()
        });

        let store = FakeStore::new();
        let make_chunk = |text: &str| {
            let span = Span::new(0, text.len());
            ChunkRecord {
                id: chunk_id(&doc_id, 0, text, 0),
                resource_id: doc_id.clone(),
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
                ingestor_kind: "path".to_string(),
                mime: None,
                uri: doc_uri.to_string(),
                metadata: metadata.clone(),
                block_seq: 0,
                seq_in_block: 0,
                block_kind: None,
                window_block_seqs: vec![],
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
        let args = GetDocumentArgs {
            id: doc_id.clone(),
            uri: None,
        };

        let result = tool_get_document(&stores, args).await;
        assert_ne!(result.is_error, Some(true));
        assert_eq!(result.content.len(), 1);

        let rendered_text = text_of(&result);

        let expected = serde_json::json!({
            "resource_id": doc_id,
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

        assert_eq!(rendered_text, expected);
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

    fn make_chunk(id: &str, resource_id: &str, store_id: &str, text: &str) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            resource_id: resource_id.to_string(),
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
            ingestor_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: format!("file:///docs/{resource_id}.md"),
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            window_block_seqs: vec![],
        }
    }

    fn text_of(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    fn search_args(query: &str) -> SearchArgs {
        SearchArgs {
            query: query.to_string(),
            stores: None,
            limit: None,
            content_length: None,
        }
    }

    fn get_chunks_args(resource_id: &str) -> GetChunksArgs {
        GetChunksArgs {
            resource_id: resource_id.to_string(),
            offset: None,
            limit: None,
        }
    }

    // -----------------------------------------------------------------------
    // E4 — search rejects limit=0
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn search_tool_rejects_limit_zero() {
        let store = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "mystore"), Box::new(store));
        let embedder = FakeEmbedder::new(128);
        let mut args = search_args("hello");
        args.limit = Some(0);
        let result = tool_search(&[av], &embedder, args).await;
        assert_eq!(
            result.is_error,
            Some(true),
            "limit=0 should produce an error result"
        );
        let text = text_of(&result);
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

    #[test]
    fn resolve_search_limit_zero_passes_through() {
        // resolve_search_limit does not reject limit=0 itself (that's the
        // tool's job) — 0 must survive unchanged so the tool-level guard fires.
        assert_eq!(resolve_search_limit(Some(0)), 0);
    }

    #[test]
    fn resolve_search_limit_negative_falls_back_to_default() {
        // Mirrors the old raw-JSON `Value::as_u64()` parse, which failed on
        // negative numbers and silently defaulted.
        assert_eq!(resolve_search_limit(Some(-5)), SEARCH_DEFAULT_LIMIT);
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

        let args = GetDocumentArgs {
            id: "doc-mismatched".to_string(),
            uri: None,
        };
        let result = tool_get_document(&[av], args).await;

        // The tool should hide the document (not leak existence) and return not-found.
        assert_eq!(
            result.is_error,
            Some(true),
            "mismatched store_id should cause resource_not_found"
        );
        let text = text_of(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("error body is JSON");
        assert_eq!(
            parsed["error"]["code"].as_str().unwrap(),
            "resource_not_found",
        );
    }

    #[tokio::test]
    async fn get_document_succeeds_when_store_id_matches() {
        let fake = FakeStore::new();
        let chunk = make_chunk("chunk-1", "doc-1", "store-A", "hello world");
        fake.upsert_chunks(vec![chunk]).await.unwrap();

        let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

        let args = GetDocumentArgs {
            id: "doc-1".to_string(),
            uri: None,
        };
        let result = tool_get_document(&[av], args).await;

        assert_ne!(
            result.is_error,
            Some(true),
            "matching store_id should succeed"
        );
        let text = text_of(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("success body is JSON");
        assert_eq!(parsed["resource_id"].as_str().unwrap(), "doc-1");
        assert!(
            parsed.get("metadata").is_some(),
            "metadata field must be present"
        );
    }

    #[tokio::test]
    async fn get_document_metadata_carries_through() {
        let fake = FakeStore::new();
        let mut chunk = make_chunk("chunk-1", "doc-meta", "store-A", "text content");
        chunk.metadata =
            localdb_core::metadata::Metadata::Document(localdb_core::metadata::DocumentMetadata {
                dublin_core: localdb_core::metadata::DublinCoreMetadata {
                    title: Some("Rich Doc".to_string()),
                    creator: vec!["Carol".to_string()],
                    date: Some("2026-05-01".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            });
        fake.upsert_chunks(vec![chunk]).await.unwrap();

        let av = AvailableStore::new(make_descriptor("store-A", "store-a"), Box::new(fake));

        let args = GetDocumentArgs {
            id: "doc-meta".to_string(),
            uri: None,
        };
        let result = tool_get_document(&[av], args).await;

        assert_ne!(result.is_error, Some(true));
        let text = text_of(&result);
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
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
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

        let args = search_args("totally absent term xyzzy");
        let result = tool_search(&[av], &embedder, args).await;
        // Should NOT be an error — just empty citations.
        assert_ne!(
            result.is_error,
            Some(true),
            "empty results should not be an error"
        );
    }

    #[tokio::test]
    async fn get_document_empty_id_returns_typed_error() {
        let fake = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));

        // Empty 'id', no 'uri' either. `id` is `#[serde(default)]` (see
        // args.rs), not schema-required, so an omitted `id` reaches this
        // same tool-level "must not be empty" path rather than failing at
        // deserialization — this exercises that path directly.
        let args = GetDocumentArgs {
            id: String::new(),
            uri: None,
        };
        let result = tool_get_document(&[av], args).await;
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
    }

    #[tokio::test]
    async fn get_document_empty_id_with_uri_mentions_search_result() {
        let fake = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));

        let args = GetDocumentArgs {
            id: String::new(),
            uri: Some("file:///docs/guide.md".to_string()),
        };
        let result = tool_get_document(&[av], args).await;
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
        assert!(
            parsed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not supported in v1"),
            "message should point the caller at 'id' from a search result"
        );
    }

    #[tokio::test]
    async fn search_unknown_store_returns_typed_error() {
        let fake = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "real-store"), Box::new(fake));
        let embedder = FakeEmbedder::new(128);

        let mut args = search_args("hello");
        args.stores = Some(vec!["nonexistent-store".to_string()]);
        let result = tool_search(&[av], &embedder, args).await;
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
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
            metadata::{DocumentMetadata, DublinCoreMetadata, Metadata},
            types::Span,
        };
        localdb_core::citation::Citation {
            chunk_id: "c1".to_string(),
            resource_id: "d1".to_string(),
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
            metadata: Metadata::Document(DocumentMetadata {
                dublin_core: DublinCoreMetadata {
                    creator,
                    date,
                    ..Default::default()
                },
                ..Default::default()
            }),
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
    fn render_citations_text_snaps_to_sentence_boundary() {
        let mut c = make_citation_with_metadata("file:///a.md", vec![], None);
        c.snippet = "This is sentence one. This is sentence two that keeps going and going and going further."
            .to_string();
        let text = render_citations_text(&[c], 25);
        let snippet_line = text
            .lines()
            .find(|l| l.trim_start().starts_with("This"))
            .unwrap()
            .trim();
        assert!(
            snippet_line.ends_with('…'),
            "expected ellipsis marker, got: {snippet_line}"
        );
        // The char immediately before the ellipsis must be the sentence
        // terminator, not a mid-word letter.
        let before_ellipsis = snippet_line
            .chars()
            .rev()
            .nth(1)
            .expect("snippet should have content before the ellipsis");
        assert_eq!(
            before_ellipsis, '.',
            "expected sentence-boundary cut, got: {snippet_line}"
        );
    }

    #[test]
    fn search_args_default_content_length() {
        assert_eq!(
            resolve_content_length(None),
            400,
            "default content_length should be 400"
        );
    }

    #[test]
    fn search_args_custom_content_length() {
        assert_eq!(resolve_content_length(Some(50)), 50);
    }

    // -----------------------------------------------------------------------
    // GetChunksArgs pagination resolution
    // -----------------------------------------------------------------------

    #[test]
    fn get_chunks_args_limit_clamped_to_max() {
        let mut args = get_chunks_args("doc-1");
        args.limit = Some(9999);
        let (_, limit) = resolve_get_chunks_pagination(&args).expect("should parse");
        assert_eq!(limit, 200, "limit should be clamped to MAX_LIMIT=200");
    }

    #[test]
    fn get_chunks_args_zero_limit_is_invalid_request() {
        // The schema requires limit >= 1; an explicit 0 must be rejected rather
        // than clamped up to 1 (which would return a chunk the caller did not
        // ask for).
        let mut args = get_chunks_args("doc-1");
        args.limit = Some(0);
        assert_invalid_request(resolve_get_chunks_pagination(&args));
    }

    #[test]
    fn get_chunks_args_defaults() {
        let args = get_chunks_args("doc-1");
        let (offset, limit) = resolve_get_chunks_pagination(&args).expect("should parse");
        assert_eq!(offset, 0, "default offset should be 0");
        assert_eq!(limit, 50, "default limit should be 50");
    }

    #[tokio::test]
    async fn get_chunks_empty_resource_id_is_invalid_request() {
        let fake = FakeStore::new();
        let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));
        let args = get_chunks_args("   ");
        let result = tool_get_chunks(&[av], args).await;
        assert_eq!(result.is_error, Some(true));
        let text = text_of(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
    }

    /// Assert a `resolve_get_chunks_pagination` failure carries the `invalid_request` code.
    fn assert_invalid_request(result: Result<(usize, usize), CallToolResult>) {
        let err = result.expect_err("expected an error result");
        assert_eq!(err.is_error, Some(true));
        let text = err.content[0].as_text().unwrap().text.clone();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
    }

    #[test]
    fn get_chunks_args_negative_offset_is_invalid_request() {
        // A present-but-negative offset must be rejected, not silently defaulted to 0.
        let mut args = get_chunks_args("doc-1");
        args.offset = Some(-1);
        assert_invalid_request(resolve_get_chunks_pagination(&args));
    }

    #[test]
    fn get_chunks_args_negative_limit_is_invalid_request() {
        // A present-but-negative limit must be rejected, not silently defaulted.
        let mut args = get_chunks_args("doc-1");
        args.limit = Some(-5);
        assert_invalid_request(resolve_get_chunks_pagination(&args));
    }

    #[test]
    fn render_citations_empty() {
        let text = render_citations_text(&[], 400);
        assert_eq!(text, "No results found.");
    }
}
