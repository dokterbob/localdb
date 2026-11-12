//! Shared read model for a single document, built on top of [`StoreBackend`]
//! and [`RetrievalStore`].
//!
//! `document list` / `document get` need the same lookup, scope, and
//! text-reconstruction logic across every surface (CLI, daemon HTTP, MCP).
//! This module is the one place that logic lives — each surface still owns
//! its own wire shape (`DocumentDetail` is deliberately not `Serialize`).

use crate::backend::{DocumentInfo, StoreBackend};
use crate::block::Block;
use crate::store::ChunkRecord;
use crate::Error;

#[cfg(test)]
mod tests;

/// A document's registry metadata plus, optionally, its reconstructed full
/// text.
///
/// Internal to the read model — deliberately NOT `Serialize`. Each surface
/// (HTTP `DocumentRecord`, MCP's `document_json`, the CLI's own rendering)
/// keeps its own wire shape built from this.
#[derive(Debug)]
pub struct DocumentDetail {
    pub info: DocumentInfo,
    pub text: Option<String>,
    /// The document's chunk count, carried out of the same chunk fetch that
    /// builds `text` — `Some(chunks.len())` when `include_text` was true,
    /// `None` when it was false (chunks were never fetched, so no count is
    /// available). Surfaces that need a chunk count for `include_text: true`
    /// lookups read it from here instead of re-fetching the chunk list.
    pub chunk_count: Option<usize>,
}

/// Look up a document plus, when `include_text` is set, its reconstructed
/// full text.
///
/// `store_id`: `Some(_)` scopes the lookup to that store (SQL-scoped,
/// unambiguous); `None` looks up the id across every store, preserving the
/// existing cross-store ambiguity error when more than one store holds a
/// document with that id (see `StoreBackend::find_document`).
///
/// Text reconstruction only runs when the document is found and
/// `include_text` is true — the cost of fetching chunks/blocks is paid only
/// when the caller actually wants the text.
pub async fn get_document_detail(
    backend: &dyn StoreBackend,
    doc_id: &str,
    store_id: Option<&str>,
    include_text: bool,
) -> Result<DocumentDetail, Error> {
    let info = backend
        .find_document(doc_id, store_id)
        .await?
        .ok_or_else(|| Error::ResourceNotFound {
            id: doc_id.to_string(),
        })?;
    fetch_detail(backend, info, include_text).await
}

/// Like [`get_document_detail`], but scoped to a caller-visible set of store
/// ids rather than a single optional one — the shared 0/1/many semantics
/// used wherever a surface has already resolved "which stores can this
/// caller see" into a list.
///
/// - Empty slice: unscoped lookup (identical to `store_id: None` on
///   `get_document_detail`) — keeps the existing global ambiguity error
///   behavior when the id exists in multiple stores.
/// - Exactly one id: SQL-scoped lookup via `store_id: Some(_)` — no
///   ambiguity path, since the query itself restricts to that store.
/// - More than one id: a `store_id: Some(_)`-scoped lookup against each
///   allowed id in turn — never the global unscoped lookup. A document that
///   exists only in stores outside this set reads as `Error::ResourceNotFound`,
///   identically to one that doesn't exist anywhere; the cross-store
///   ambiguity error fires only when two or more *allowed* stores hold the
///   id. Text/chunk fetching (when `include_text`) runs only once the
///   lookup has resolved to exactly one allowed store, never speculatively
///   against a candidate the caller cannot see.
pub async fn get_document_detail_scoped(
    backend: &dyn StoreBackend,
    doc_id: &str,
    allowed_store_ids: &[String],
    include_text: bool,
) -> Result<DocumentDetail, Error> {
    match allowed_store_ids {
        [] => get_document_detail(backend, doc_id, None, include_text).await,
        [only] => get_document_detail(backend, doc_id, Some(only.as_str()), include_text).await,
        many => {
            let mut found: Option<DocumentInfo> = None;
            for store_id in many {
                if let Some(info) = backend.find_document(doc_id, Some(store_id)).await? {
                    if found.is_some() {
                        return Err(Error::InvalidRequest {
                            message: format!(
                                "document '{doc_id}' exists in multiple stores; use \
                                 store-scoped search to disambiguate"
                            ),
                        });
                    }
                    found = Some(info);
                }
            }
            let info = found.ok_or_else(|| Error::ResourceNotFound {
                id: doc_id.to_string(),
            })?;
            fetch_detail(backend, info, include_text).await
        }
    }
}

/// Fetch, when `include_text` is set, `info`'s reconstructed full text and
/// chunk count — the tail shared by every `get_document_detail*` lookup once
/// the document has already been resolved to exactly one store.
async fn fetch_detail(
    backend: &dyn StoreBackend,
    info: DocumentInfo,
    include_text: bool,
) -> Result<DocumentDetail, Error> {
    let (text, chunk_count) = if include_text {
        let store = backend.retrieval_store(&info.store_id).await?;
        let chunks = store.get_chunks_for_resource(&info.id).await?;
        let blocks = store.get_blocks_for_resource(&info.id).await?;
        let text = reconstruct_document_text(&chunks, &blocks);
        (Some(text), Some(chunks.len()))
    } else {
        (None, None)
    };

    Ok(DocumentDetail {
        info,
        text,
        chunk_count,
    })
}

/// Reconstruct a document's full text from its persisted blocks, falling
/// back to joining chunk texts when no blocks were persisted.
///
/// Blocks are the canonical source of truth (each block's text is stored
/// exactly once); chunks can duplicate content (e.g. the table chunker
/// re-emits the header/separator row in every chunk of a multi-chunk table,
/// specs/04-search-pipeline.md §3, intentional). Blocks are joined with
/// `"\n\n"` — matching the blank-line separation Markdown extraction strips
/// out between sibling blocks; chunk texts, when there are no blocks, are
/// joined with `"\n"`.
pub fn reconstruct_document_text(chunks: &[ChunkRecord], blocks: &[Block]) -> String {
    if blocks.is_empty() {
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
    }
}
