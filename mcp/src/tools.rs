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
    Embedder, SEARCH_MAX_LIMIT,
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
    match find_document_chunks(stores, &args.id, args.store.as_deref()).await {
        Ok(Some((store, chunks))) => {
            let resource_id = chunks[0].resource_id.clone();
            let blocks = match store.store.get_blocks_for_resource(&resource_id).await {
                Ok(blocks) => blocks,
                Err(e) => {
                    return typed_error(
                        e.code(),
                        format!(
                            "error fetching blocks for document from store '{}': {e}",
                            store.descriptor.name
                        ),
                    )
                }
            };
            success_json(&document_json(store, &chunks, &blocks))
        }
        Ok(None) => typed_error(
            "resource_not_found",
            format!("no document with id '{}' found in any store", args.id),
        ),
        Err(result) => result,
    }
}

/// Look up a document's chunks by id, optionally scoped to a single store.
///
/// `store_filter`, when present, is a store id or name (#144) — e.g. the
/// `store.id`/`store.name` from a prior `search` citation. It is resolved via
/// [`select_mcp_stores`] (the same id-or-name resolver `search`'s `stores`
/// argument uses) rather than a parallel matcher, so an unknown store id/name
/// produces the same `store_not_found` error shape as `search`. Once
/// resolved, the scan below is restricted to that single store; an absent
/// `store_filter` keeps the pre-#144 behavior of scanning every available
/// store and returning whichever matches first.
async fn find_document_chunks<'a>(
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

/// Build the `get_document` JSON payload.
///
/// `text` is reconstructed from `blocks` when available — each block's
/// canonical text is stored exactly once (see
/// `RetrievalStore::get_blocks_for_resource`), so joining these avoids the
/// duplicated header/separator rows that joining `ChunkRecord.text` produces
/// for a multi-chunk table (the table chunker intentionally re-emits the
/// header + separator in every chunk, spec 04 §3) — and likewise avoids
/// duplicating overlapping turns across message-window chunks (#129). Blocks
/// are joined with `"\n\n"`, matching the blank-line separation Markdown
/// extraction strips out between sibling blocks (the same separator
/// `chunker.rs`'s message-window path already uses when it joins multiple
/// blocks' texts into one chunk).
///
/// Falls back to the legacy chunk-text join when `blocks` is empty: rows
/// indexed before the Resource/Block architecture existed never persisted
/// blocks, and `FakeStore`-backed tests that only call `upsert_chunks`
/// (not `upsert_chunks_and_blocks`/`upsert_blocks`) have none either. All
/// other fields (title, metadata, uri, chunk_count, etc.) always come from
/// the chunk records regardless of which path produced `text`.
fn document_json(
    store: &AvailableStore,
    chunks: &[localdb_core::ChunkRecord],
    blocks: &[localdb_core::block::Block],
) -> Value {
    let first = &chunks[0];
    let full_text = if blocks.is_empty() {
        chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    };
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

/// Resolve `GetChunksArgs::limit` to a validated `usize`.
///
/// Distinguishes absent (→ default) from present-but-invalid (→ error): an
/// explicit `limit: 0` or a negative value is a tool-level `invalid_request`
/// error rather than a silent default or clamp (clamping `0` up to `1` would
/// silently return a chunk the caller did not ask for). A valid `limit` is
/// capped at `GET_CHUNKS_MAX_LIMIT`.
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

/// Resolve `GetChunksArgs::offset`/`limit` to validated `usize`s.
///
/// A thin wrapper over [`resolve_offset`]/[`resolve_limit`], kept only for
/// their dedicated unit tests below (`tool_get_chunks` itself calls the two
/// underlying functions separately, since the anchor path needs `limit`
/// resolved before `offset` even applies).
#[cfg(test)]
fn resolve_get_chunks_pagination(args: &GetChunksArgs) -> Result<(usize, usize), CallToolResult> {
    let offset = resolve_offset(args.offset)?;
    let limit = resolve_limit(args.limit)?;
    Ok((offset, limit))
}

/// Anchor-relative pagination (#146): `offset`, `anchor_chunk_id`, and
/// `anchor_block_seq` are pairwise mutually exclusive — specifying more than
/// one is a tool-level `invalid_request` error, not a silent precedence rule.
/// See specs/05-surfaces.md §4.1.
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
    match find_document_chunks(stores, &args.resource_id, args.store.as_deref()).await {
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
                page: None,
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
            store: None,
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

    /// Regression test: `get_document` must reconstruct a multi-chunk table
    /// from its persisted `blocks`, not by joining `ChunkRecord.text`. The
    /// table chunker (spec 04 §3, intentional) re-emits the header +
    /// `|---|` separator row in every chunk of a table split across
    /// multiple chunks — joining chunk texts would duplicate that header
    /// once per chunk. The single `Table` block holds the canonical text
    /// with the header exactly once, so reconstruction from blocks must not
    /// duplicate it.
    #[tokio::test]
    async fn tool_get_document_reconstructs_table_without_duplicated_header() {
        use localdb_core::block::{Block, BlockKind};
        use localdb_core::{chunk_blocks, CharSizer, ChunkerConfig};

        let store_id = new_ulid();
        let doc_uri = "file:///docs/table.md";

        // Same fixture shape as chunker.rs's own
        // `table_multi_chunk_split_preserves_header` unit test: with
        // target_tokens=40 and CharSizer, 2 data rows pack per chunk, so 10
        // rows split into 5 chunks, each re-emitting the header/separator.
        let table_text = {
            let mut md = String::from("| A | B |\n|---|---|\n");
            let rows: Vec<String> = (0..10).map(|i| format!("| {i} | {i} |")).collect();
            md.push_str(&rows.join("\n"));
            md
        };
        let doc_hash = content_hash(&table_text);
        let doc_id = resource_id(doc_uri, &doc_hash);

        let block = Block {
            seq: 0,
            kind: BlockKind::Table {
                headers: vec!["A".to_string(), "B".to_string()],
                rows: 10,
            },
            text: table_text.clone(),
            location: None,
        };

        let cfg = ChunkerConfig {
            preset: "prose".to_string(),
            target_tokens: Some(40),
            overlap_tokens: Some(0),
            window_turns: None,
            stride_turns: None,
        };
        let chunk_outputs = chunk_blocks(&doc_id, std::slice::from_ref(&block), &cfg, &CharSizer)
            .expect("chunking the table fixture must succeed");
        assert!(
            chunk_outputs.len() >= 2,
            "fixture must produce a multi-chunk table split, got {} chunk(s)",
            chunk_outputs.len()
        );

        let metadata = Metadata::default();
        let store = FakeStore::new();
        let chunk_records: Vec<ChunkRecord> = chunk_outputs
            .iter()
            .map(|co| ChunkRecord {
                id: co.id.clone(),
                resource_id: doc_id.clone(),
                store_id: store_id.clone(),
                text: co.text.clone(),
                span: co.span.clone(),
                heading_path: co.heading_path.clone(),
                embedding: vec![0.1, 0.2],
                policy_version: "policy-v1".to_string(),
                fetched_at: "2026-06-29T00:00:00Z".to_string(),
                content_hash: doc_hash.clone(),
                origin_store: store_id.clone(),
                source_id: "src-1".to_string(),
                ingestor_kind: "path".to_string(),
                mime: Some("text/markdown".to_string()),
                uri: doc_uri.to_string(),
                metadata: metadata.clone(),
                block_seq: co.block_seq,
                seq_in_block: co.seq_in_block,
                block_kind: co.block_kind.clone(),
                page: None,
                window_block_seqs: co.window_block_seqs.clone(),
            })
            .collect();

        store
            .upsert_chunks_and_blocks(&store_id, &doc_id, chunk_records, &[block], None)
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
            store: None,
        };

        let result = tool_get_document(&stores, args).await;
        assert_ne!(result.is_error, Some(true));
        let rendered_text = text_of(&result);
        let parsed: Value = serde_json::from_str(&rendered_text).unwrap();
        let reconstructed = parsed["text"].as_str().unwrap();

        assert_eq!(
            reconstructed.matches("| A | B |").count(),
            1,
            "reconstructed text must contain the table header exactly once, \
             not once per chunk; got: {reconstructed:?}"
        );
        assert_eq!(
            reconstructed.matches("|---|---|").count(),
            1,
            "reconstructed text must contain the separator row exactly once; \
             got: {reconstructed:?}"
        );
        assert_eq!(
            reconstructed, table_text,
            "block-based reconstruction should equal the canonical block text exactly"
        );
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
            page: None,
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
            anchor_chunk_id: None,
            anchor_block_seq: None,
            store: None,
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
            store: None,
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
            store: None,
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
            store: None,
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
            store: None,
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
            store: None,
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
    // #144 — `store` discriminator on get_document / get_chunks
    // -----------------------------------------------------------------------

    /// Build two `AvailableStore`s that each hold a chunk for the *same*
    /// `doc_id`, with distinguishable text, so a caller can tell which
    /// store's copy a lookup returned.
    async fn duplicate_doc_stores(doc_id: &str) -> (AvailableStore, AvailableStore) {
        let store_a = FakeStore::new();
        let chunk_a = make_chunk("chunk-a", doc_id, "store-A-id", "from store A");
        store_a.upsert_chunks(vec![chunk_a]).await.unwrap();
        let av_a = AvailableStore::new(make_descriptor("store-A-id", "store-a"), Box::new(store_a));

        let store_b = FakeStore::new();
        let chunk_b = make_chunk("chunk-b", doc_id, "store-B-id", "from store B");
        store_b.upsert_chunks(vec![chunk_b]).await.unwrap();
        let av_b = AvailableStore::new(make_descriptor("store-B-id", "store-b"), Box::new(store_b));

        (av_a, av_b)
    }

    #[tokio::test]
    async fn get_document_with_store_name_disambiguates_duplicate_id_across_stores() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let mut args_a = GetDocumentArgs {
            id: "dup-doc".to_string(),
            uri: None,
            store: None,
        };
        args_a.store = Some("store-a".to_string());
        let result_a = tool_get_document(&stores, args_a).await;
        assert_ne!(result_a.is_error, Some(true));
        let parsed_a: serde_json::Value = serde_json::from_str(&text_of(&result_a)).unwrap();
        assert_eq!(parsed_a["text"].as_str().unwrap(), "from store A");
        assert_eq!(parsed_a["store"]["name"].as_str().unwrap(), "store-a");

        let args_b = GetDocumentArgs {
            id: "dup-doc".to_string(),
            uri: None,
            store: Some("store-b".to_string()),
        };
        let result_b = tool_get_document(&stores, args_b).await;
        assert_ne!(result_b.is_error, Some(true));
        let parsed_b: serde_json::Value = serde_json::from_str(&text_of(&result_b)).unwrap();
        assert_eq!(parsed_b["text"].as_str().unwrap(), "from store B");
        assert_eq!(parsed_b["store"]["name"].as_str().unwrap(), "store-b");
    }

    #[tokio::test]
    async fn get_document_with_store_id_also_disambiguates() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let args = GetDocumentArgs {
            id: "dup-doc".to_string(),
            uri: None,
            store: Some("store-B-id".to_string()),
        };
        let result = tool_get_document(&stores, args).await;
        assert_ne!(result.is_error, Some(true));
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(parsed["text"].as_str().unwrap(), "from store B");
        assert_eq!(parsed["store"]["id"].as_str().unwrap(), "store-B-id");
    }

    #[tokio::test]
    async fn get_document_unknown_store_returns_store_not_found() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let args = GetDocumentArgs {
            id: "dup-doc".to_string(),
            uri: None,
            store: Some("no-such-store".to_string()),
        };
        let result = tool_get_document(&stores, args).await;
        assert_eq!(result.is_error, Some(true));
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
    }

    #[tokio::test]
    async fn get_document_omitted_store_keeps_first_match_backward_compat() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let args = GetDocumentArgs {
            id: "dup-doc".to_string(),
            uri: None,
            store: None,
        };
        let result = tool_get_document(&stores, args).await;
        assert_ne!(result.is_error, Some(true));
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(
            parsed["text"].as_str().unwrap(),
            "from store A",
            "omitted store must keep pre-#144 first-match-wins behavior"
        );
    }

    #[tokio::test]
    async fn get_chunks_with_store_name_disambiguates_duplicate_id_across_stores() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let mut args_a = get_chunks_args("dup-doc");
        args_a.store = Some("store-a".to_string());
        let result_a = tool_get_chunks(&stores, args_a).await;
        assert_ne!(result_a.is_error, Some(true));
        let parsed_a: serde_json::Value = serde_json::from_str(&text_of(&result_a)).unwrap();
        assert_eq!(
            parsed_a["chunks"][0]["text"].as_str().unwrap(),
            "from store A"
        );
        assert_eq!(parsed_a["store"]["name"].as_str().unwrap(), "store-a");

        let mut args_b = get_chunks_args("dup-doc");
        args_b.store = Some("store-b".to_string());
        let result_b = tool_get_chunks(&stores, args_b).await;
        assert_ne!(result_b.is_error, Some(true));
        let parsed_b: serde_json::Value = serde_json::from_str(&text_of(&result_b)).unwrap();
        assert_eq!(
            parsed_b["chunks"][0]["text"].as_str().unwrap(),
            "from store B"
        );
        assert_eq!(parsed_b["store"]["name"].as_str().unwrap(), "store-b");
    }

    #[tokio::test]
    async fn get_chunks_with_store_id_also_disambiguates() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let mut args = get_chunks_args("dup-doc");
        args.store = Some("store-A-id".to_string());
        let result = tool_get_chunks(&stores, args).await;
        assert_ne!(result.is_error, Some(true));
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(
            parsed["chunks"][0]["text"].as_str().unwrap(),
            "from store A"
        );
        assert_eq!(parsed["store"]["id"].as_str().unwrap(), "store-A-id");
    }

    #[tokio::test]
    async fn get_chunks_unknown_store_returns_store_not_found() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let mut args = get_chunks_args("dup-doc");
        args.store = Some("no-such-store".to_string());
        let result = tool_get_chunks(&stores, args).await;
        assert_eq!(result.is_error, Some(true));
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
    }

    #[tokio::test]
    async fn get_chunks_omitted_store_keeps_first_match_backward_compat() {
        let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
        let stores = vec![av_a, av_b];

        let args = get_chunks_args("dup-doc");
        let result = tool_get_chunks(&stores, args).await;
        assert_ne!(result.is_error, Some(true));
        let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(
            parsed["chunks"][0]["text"].as_str().unwrap(),
            "from store A",
            "omitted store must keep pre-#144 first-match-wins behavior"
        );
    }

    // -----------------------------------------------------------------------
    // Codex review round 2, finding 3 — select_mcp_stores id-vs-name ambiguity
    // -----------------------------------------------------------------------

    #[test]
    fn select_mcp_stores_id_match_wins_over_shadowing_name() {
        // stores[0] is *named* the same string as stores[1]'s *id*. An
        // order-dependent `name == x || id == x` predicate would return
        // stores[0] (it comes first and matches on the name arm); the fix
        // must do an id pass before falling back to a name pass, so the more
        // specific (unique, machine-generated) id match wins regardless of
        // slice order.
        let shared = "shadow-value".to_string();
        let store_0 = AvailableStore::new(
            make_descriptor("store-0-id", &shared),
            Box::new(FakeStore::new()),
        );
        let store_1 = AvailableStore::new(
            make_descriptor(&shared, "store-1-name"),
            Box::new(FakeStore::new()),
        );
        let stores = vec![store_0, store_1];

        let selected = select_mcp_stores(&stores, std::slice::from_ref(&shared))
            .expect("lookup should resolve");
        assert_eq!(selected.len(), 1);
        assert_eq!(
            selected[0].id, shared,
            "the id match (stores[1]) must win over the shadowing name match (stores[0])"
        );
        assert_eq!(selected[0].name, "store-1-name");
    }

    #[test]
    fn select_mcp_stores_falls_back_to_name_when_no_id_matches() {
        // Ordinary name-lookup path: no store's id equals the lookup string,
        // so the name pass must find it.
        let store_0 =
            AvailableStore::new(make_descriptor("id-0", "alpha"), Box::new(FakeStore::new()));
        let store_1 =
            AvailableStore::new(make_descriptor("id-1", "beta"), Box::new(FakeStore::new()));
        let stores = vec![store_0, store_1];

        let selected = select_mcp_stores(&stores, std::slice::from_ref(&"beta".to_string()))
            .expect("lookup should resolve by name");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "id-1");
        assert_eq!(selected[0].name, "beta");
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
            citation::{
                ChunkPosition, CitationBlock, CitationLocation, CitationProvenance, CitationStore,
                Score,
            },
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
            block: CitationBlock {
                seq: 0,
                kind: None,
                page: None,
            },
            chunk_position: ChunkPosition { seq_in_block: 0 },
            location: CitationLocation {
                span: Span::new(0, 4),
                window_block_seqs: vec![],
            },
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
