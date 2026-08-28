//! MCP tool implementations: search, get_document, get_chunks, list_stores,
//! list_documents.
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
    get_document_detail,
    search::{QueryRequest, QueryResponse, SearchOrchestrator, StoreHandle},
    store::{RetrievalStore, StoreStats},
    DocumentDetail, Embedder, Error, StoreBackend, SEARCH_MAX_LIMIT,
};

use crate::args::{GetChunksArgs, GetDocumentArgs, ListDocumentsArgs, SearchArgs};

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
const SEARCH_DEFAULT_CONTENT_LENGTH: usize = 400;

/// Resolve `SearchArgs::limit` to a `usize`, preserving the pre-rmcp
/// behavior: absent -> default; a valid non-negative integer -> clamped to
/// `SEARCH_MAX_LIMIT` (`localdb_core::SEARCH_MAX_LIMIT` — shared with the
/// HTTP `/v1/search` clamp and the CLI's embedded search, issue #187
/// review); a negative integer -> silently falls back to the default
/// (mirroring the old raw-JSON `Value::as_u64()` parse, which simply failed
/// to match on negative numbers and fell through to
/// `unwrap_or(DEFAULT_LIMIT)`). An explicit `0` passes through unchanged so
/// the tool-level guard in `tool_search` can reject it.
///
/// This does not call `localdb_core::clamp_search_limit` directly: that
/// helper's signature is `usize -> usize`, but this function's input is an
/// `Option<i64>` with its own absent/negative-handling semantics — the
/// `usize::try_from` conversion has to happen first, so the shared piece is
/// just the `SEARCH_MAX_LIMIT` constant.
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
/// If `stores` is non-empty, only those stores are queried — each entry may
/// be a store id or a store name (#144: this lets a caller round-trip the
/// `store.id`/`store.name` from a prior `search` citation straight back in).
/// Unknown store id/name → returns a tool error with code `store_not_found`.
// `CallToolResult` crossed clippy's result_large_err threshold once the
// workspace `schemars` dep gained `preserve_order` (serde_json's Map
// switches from BTreeMap to IndexMap, growing `serde_json::Value`). Boxing
// every `Err(CallToolResult)` call site in this crate is out of scope for
// that change; allow the lint on the affected functions instead.
#[allow(clippy::result_large_err)]
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
            // Ids are unique and machine-generated; names are user-chosen and
            // (per `validate_store_name`) may legitimately collide with
            // another store's id. Resolve by id first so that exact,
            // unambiguous signal always wins over a same-named but unrelated
            // store — only fall back to a name match when no id matches.
            match stores
                .iter()
                .find(|s| &s.descriptor.id == name)
                .or_else(|| stores.iter().find(|s| &s.descriptor.name == name))
            {
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
    // Validate the filters before the store lookup, alongside the other
    // argument checks above. Argument validity is a property of the call,
    // not of which stores this session happens to expose — behind the
    // empty-store return, a malformed `added_after` was reported as a
    // successful empty search rather than `invalid_request`.
    let metadata_filters = match args.filters.into_metadata_filters() {
        Ok(f) => f,
        Err(e) => return typed_error(e.code(), e.to_string()),
    };
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
        filters: metadata_filters,
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
/// Looks up a document by ID via `backend` (the shared read model,
/// `localdb_core::get_document_detail`) and returns normalized text +
/// metadata.
///
/// Returns `resource_not_found` error if no document with that id is found.
///
/// Note: URI-based lookup is not supported in v1 (the shared read model
/// looks up by id only). Callers must use a document ID obtained from a
/// prior `search` call. `id` is a required field on `GetDocumentArgs`, so a
/// caller omitting it entirely never reaches this function — rmcp's
/// `Parameters<T>` extractor fails first, which is still a tool-level error
/// (see `mcp/src/lib.rs`'s two-tier error model doc), just with a generic
/// rmcp-authored message. An explicit empty string still reaches here and is
/// rejected below (with a more specific message when `uri` was given
/// instead, preserving v1's guidance).
pub async fn tool_get_document(
    stores: &[AvailableStore],
    backend: &dyn StoreBackend,
    args: GetDocumentArgs,
) -> CallToolResult {
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

    if let Some(store_value) = &args.store {
        let handles = match select_mcp_stores(stores, std::slice::from_ref(store_value)) {
            Ok(handles) => handles,
            Err(result) => return result,
        };
        let handle = &handles[0];
        return match get_document_from_store(backend, &handle.id, &handle.name, &args.id).await {
            Ok(Some(json)) => success_json(&json),
            Ok(None) => typed_error(
                "resource_not_found",
                format!("no document with id '{}' found in any store", args.id),
            ),
            Err(result) => result,
        };
    }

    // Note: `store` is omitted. Scan the session's available stores in
    // order and return whichever holds the id first — if the id exists in
    // more than one store, which copy comes back depends on iteration
    // order, not on any tie-break rule (a plain "first match wins", not a
    // documented precedence).
    for store in stores {
        match get_document_from_store(
            backend,
            &store.descriptor.id,
            &store.descriptor.name,
            &args.id,
        )
        .await
        {
            Ok(Some(json)) => return success_json(&json),
            Ok(None) => continue,
            Err(result) => return result,
        }
    }
    typed_error(
        "resource_not_found",
        format!("no document with id '{}' found in any store", args.id),
    )
}

/// Look up one document, scoped to a single store, via the shared
/// `get_document_detail` read model — `Ok(None)` means the store has no
/// document with that id (the caller decides how to report that); any other
/// backend error becomes a ready-to-return `CallToolResult`.
///
/// `store_id` scopes the `backend` lookup (`DocumentInfo`, text, chunk
/// count); `store_name` is only used to label the store in the returned
/// JSON and in error messages.
#[allow(clippy::result_large_err)] // see note on select_mcp_stores above
async fn get_document_from_store(
    backend: &dyn StoreBackend,
    store_id: &str,
    store_name: &str,
    doc_id: &str,
) -> Result<Option<Value>, CallToolResult> {
    match get_document_detail(backend, doc_id, Some(store_id), true).await {
        Ok(detail) => Ok(Some(document_json(&detail, store_name))),
        Err(Error::ResourceNotFound { .. }) => Ok(None),
        Err(e) => Err(typed_error(
            e.code(),
            format!("error fetching document from store '{store_name}': {e}"),
        )),
    }
}

/// Build the `get_document` JSON payload from the shared read model's
/// [`DocumentDetail`].
///
/// `chunk_count` comes straight off `detail` — populated by the same chunk
/// fetch that built `text`, since `get_document_from_store` always requests
/// `include_text: true`.
fn document_json(detail: &DocumentDetail, store_name: &str) -> Value {
    serde_json::json!({
        "resource_id": detail.info.id,
        "uri": detail.info.uri,
        "title": detail.info.metadata.title(),
        "date_original": detail.info.date_original,
        "date_parsed": detail.info.date_parsed,
        "store": {
            "id": detail.info.store_id,
            "name": store_name,
        },
        "provenance": {
            "fetched_at": detail.info.fetched_at,
            "content_hash": detail.info.content_hash,
            "index_updated_at": detail.info.index_updated_at,
        },
        "metadata": detail.info.metadata,
        "chunk_count": detail.chunk_count.unwrap_or(0),
        "text": detail.text.as_deref().unwrap_or(""),
    })
}

// ---------------------------------------------------------------------------
// Tool: get_chunks
// ---------------------------------------------------------------------------

const GET_CHUNKS_DEFAULT_LIMIT: usize = 50;
const GET_CHUNKS_MAX_LIMIT: usize = 200;

/// Resolve `GetChunksArgs::limit` to a validated `usize`.
///
/// Distinguishes absent (→ default) from present-but-invalid (→ error): an
/// explicit `limit: 0` or a negative value is a tool-level `invalid_request`
/// error rather than a silent default or clamp (clamping `0` up to `1` would
/// silently return a chunk the caller did not ask for). A valid `limit` is
/// capped at `GET_CHUNKS_MAX_LIMIT`.
#[allow(clippy::result_large_err)] // see note on select_mcp_stores above
fn resolve_limit(limit: Option<i64>) -> Result<usize, CallToolResult> {
    match limit {
        None => Ok(GET_CHUNKS_DEFAULT_LIMIT),
        Some(0) => Err(typed_error(
            "invalid_request",
            "invalid arguments: 'limit' must be at least 1",
        )),
        Some(n) => usize::try_from(n)
            .map(|v| v.min(GET_CHUNKS_MAX_LIMIT))
            .map_err(|_| {
                typed_error(
                    "invalid_request",
                    "invalid arguments: 'limit' must be a positive integer",
                )
            }),
    }
}

/// Resolve `GetChunksArgs::offset` to a validated `usize` (absent → 0).
#[allow(clippy::result_large_err)] // see note on select_mcp_stores above
fn resolve_offset(offset: Option<i64>) -> Result<usize, CallToolResult> {
    match offset {
        None => Ok(0),
        Some(n) => usize::try_from(n).map_err(|_| {
            typed_error(
                "invalid_request",
                "invalid arguments: 'offset' must be a non-negative integer",
            )
        }),
    }
}

/// Anchor-relative pagination (#146): `offset`, `anchor_chunk_id`, and
/// `anchor_block_seq` are pairwise mutually exclusive — specifying more than
/// one is a tool-level `invalid_request` error, not a silent precedence rule.
/// See specs/05-surfaces.md §4.1.
#[allow(clippy::result_large_err)] // see note on select_mcp_stores above
fn check_anchor_mutual_exclusivity(args: &GetChunksArgs) -> Result<(), CallToolResult> {
    let specified_count = [
        args.offset.is_some(),
        args.anchor_chunk_id.is_some(),
        args.anchor_block_seq.is_some(),
    ]
    .into_iter()
    .filter(|&specified| specified)
    .count();

    if specified_count > 1 {
        return Err(typed_error(
            "invalid_request",
            "invalid arguments: 'offset', 'anchor_chunk_id', and 'anchor_block_seq' are mutually exclusive; pass at most one",
        ));
    }
    Ok(())
}

/// Resolve `anchor_chunk_id` to its 0-based index in `sorted_chunks` (already
/// sorted by `(block_seq, seq_in_block, ...)`): an exact `chunk_id` match.
/// Unknown id → `chunk_not_found`.
#[allow(clippy::result_large_err)] // see note on select_mcp_stores above
fn resolve_anchor_chunk_id(
    sorted_chunks: &[localdb_core::ChunkRecord],
    anchor_chunk_id: &str,
) -> Result<usize, CallToolResult> {
    sorted_chunks
        .iter()
        .position(|c| c.id == anchor_chunk_id)
        .ok_or_else(|| {
            typed_error(
                "chunk_not_found",
                format!("no chunk with id '{anchor_chunk_id}' found in this resource"),
            )
        })
}

/// Resolve `anchor_block_seq` to its 0-based index in `sorted_chunks` via
/// lower-bound: the first chunk with `block_seq >= anchor_block_seq`. Since
/// `sorted_chunks` is already ordered by `(block_seq, seq_in_block, ...)`,
/// the first position satisfying the predicate is automatically tie-broken
/// by the lowest `seq_in_block` at that `block_seq`. `anchor_block_seq` past
/// every block in the resource → `chunk_not_found`.
#[allow(clippy::result_large_err)] // see note on select_mcp_stores above
fn resolve_anchor_block_seq(
    sorted_chunks: &[localdb_core::ChunkRecord],
    anchor_block_seq: u32,
) -> Result<usize, CallToolResult> {
    sorted_chunks
        .iter()
        .position(|c| c.block_seq >= anchor_block_seq)
        .ok_or_else(|| {
            typed_error(
                "chunk_not_found",
                format!("anchor_block_seq {anchor_block_seq} is past every block in this resource"),
            )
        })
}

/// Compute the `limit`-sized page centered on `anchor_idx` within a
/// `total`-length list, clamped to the list bounds. Returns `(offset, end)`.
///
/// Per specs/05-surfaces.md §4.1: the window never shrinks below `limit`
/// purely because the anchor is near an edge — it shifts toward the
/// interior instead; it only returns fewer than `limit` chunks when
/// `total < limit`.
fn centered_window(anchor_idx: usize, total: usize, limit: usize) -> (usize, usize) {
    if total <= limit {
        return (0, total);
    }
    let half = limit / 2;
    let mut offset = anchor_idx.saturating_sub(half);
    if offset + limit > total {
        offset = total - limit;
    }
    (offset, offset + limit)
}

/// Look up a document's chunks by id, optionally scoped to a single store.
///
/// `store_filter`, when present, is a store id or name (#144) — e.g. the
/// `store.id`/`store.name` from a prior `search` citation. It is resolved via
/// [`select_mcp_stores`] (the same id-or-name resolver `search`'s `stores`
/// argument uses) rather than a parallel matcher, so an unknown store id/name
/// produces the same `store_not_found` error shape as `search`. Once
/// resolved, the scan below is restricted to that single store; an absent
/// `store_filter` scans every available store and returns whichever matches
/// first.
///
/// `get_document` resolves through the shared `localdb_core::get_document_detail`
/// read model instead of this scan (see `get_document_from_store`) — but
/// `get_chunks` still needs the full, ordered `ChunkRecord` list to paginate
/// over, which that read model doesn't expose, so it keeps this brute-force
/// scan over the session's own `RetrievalStore` handles.
#[allow(clippy::result_large_err)] // see note on select_mcp_stores above
async fn find_chunks_for_resource<'a>(
    stores: &'a [AvailableStore],
    doc_id: &str,
    store_filter: Option<&str>,
) -> Result<Option<(&'a AvailableStore, Vec<localdb_core::ChunkRecord>)>, CallToolResult> {
    let scoped: Vec<&'a AvailableStore> = match store_filter {
        Some(store_id_or_name) => {
            let handles =
                select_mcp_stores(stores, std::slice::from_ref(&store_id_or_name.to_string()))?;
            let handle = &handles[0];
            stores
                .iter()
                .filter(|s| s.descriptor.id == handle.id)
                .collect()
        }
        None => stores.iter().collect(),
    };

    for store in scoped {
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
/// **Anchor-relative pagination (#146):** as an alternative to `offset`,
/// callers may pass `anchor_chunk_id` or `anchor_block_seq` (mutually
/// exclusive with `offset` and with each other — see
/// `check_anchor_mutual_exclusivity`). Once an anchor resolves to a position
/// in the full sorted chunk list, the response window is `limit` chunks
/// centered on that position (see `centered_window`), and the response
/// carries `anchor_index` — the anchor's 0-based index within the returned
/// `chunks` array — instead of `null`. See specs/05-surfaces.md §4.1.
///
/// Note: URI-based lookup is not supported in v1, matching `get_document`.
pub async fn tool_get_chunks(stores: &[AvailableStore], args: GetChunksArgs) -> CallToolResult {
    if args.resource_id.trim().is_empty() {
        return typed_error(
            "invalid_request",
            "invalid arguments: 'resource_id' must not be empty",
        );
    }
    if let Err(result) = check_anchor_mutual_exclusivity(&args) {
        return result;
    }
    let limit = match resolve_limit(args.limit) {
        Ok(v) => v,
        Err(result) => return result,
    };
    match find_chunks_for_resource(stores, &args.resource_id, args.store.as_deref()).await {
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

            let (offset, anchor_index) = if let Some(anchor_chunk_id) = &args.anchor_chunk_id {
                match resolve_anchor_chunk_id(&chunks, anchor_chunk_id) {
                    Ok(idx) => {
                        let (offset, _end) = centered_window(idx, chunks.len(), limit);
                        (offset, Some(idx - offset))
                    }
                    Err(result) => return result,
                }
            } else if let Some(anchor_block_seq) = args.anchor_block_seq {
                match resolve_anchor_block_seq(&chunks, anchor_block_seq) {
                    Ok(idx) => {
                        let (offset, _end) = centered_window(idx, chunks.len(), limit);
                        (offset, Some(idx - offset))
                    }
                    Err(result) => return result,
                }
            } else {
                match resolve_offset(args.offset) {
                    Ok(offset) => (offset, None),
                    Err(result) => return result,
                }
            };

            success_json(&chunks_json(store, &chunks, offset, limit, anchor_index))
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
    anchor_index: Option<usize>,
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
        "anchor_index": anchor_index,
        "chunks": page,
    })
}

// ---------------------------------------------------------------------------
// Tool: list_documents
// ---------------------------------------------------------------------------

/// Execute the `list_documents` tool.
///
/// Lists every document registered in `args.store` (required — an id or
/// name, resolved via [`select_mcp_stores`] exactly like every other tool's
/// store argument), optionally filtered to `args.source`, paginated by
/// `args.offset`/`args.limit` (the same `resolve_offset`/`resolve_limit`
/// helpers `get_chunks` uses, including their defaults and range errors).
///
/// An unknown `store` is `store_not_found`, matching `search`/`get_document`.
/// An unknown `source` is a pure filter — it yields an empty `documents`
/// list, not an error, matching `StoreBackend::list_documents`'s contract.
pub async fn tool_list_documents(
    stores: &[AvailableStore],
    backend: &dyn StoreBackend,
    args: ListDocumentsArgs,
) -> CallToolResult {
    let handles = match select_mcp_stores(stores, std::slice::from_ref(&args.store)) {
        Ok(handles) => handles,
        Err(result) => return result,
    };
    let handle = &handles[0];

    let offset = match resolve_offset(args.offset) {
        Ok(v) => v,
        Err(result) => return result,
    };
    let limit = match resolve_limit(args.limit) {
        Ok(v) => v,
        Err(result) => return result,
    };

    let total = match backend
        .count_documents(&handle.id, args.source.as_deref())
        .await
    {
        Ok(n) => n,
        Err(e) => {
            return typed_error(
                e.code(),
                format!("failed to count documents in store '{}': {e}", handle.name),
            )
        }
    };

    let page = match backend
        .list_documents(&handle.id, args.source.as_deref(), Some(limit), offset)
        .await
    {
        Ok(docs) => docs,
        Err(e) => {
            return typed_error(
                e.code(),
                format!("failed to list documents in store '{}': {e}", handle.name),
            )
        }
    };
    let returned = page.len();
    let documents: Vec<Value> = page
        .iter()
        .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
        .collect();

    success_json(&serde_json::json!({
        "store": {
            "id": handle.id,
            "name": handle.name,
        },
        "total": total,
        "offset": offset,
        "limit": limit,
        "returned": returned,
        "documents": documents,
    }))
}

mod stores_backend;
pub use stores_backend::StoresBackend;

#[cfg(test)]
mod tests;
