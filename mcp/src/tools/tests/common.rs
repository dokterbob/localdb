//! Shared test helpers for `tools` tests.

use std::sync::Arc;

use rmcp::model::CallToolResult;

use localdb_core::store::{FakeStore, RetrievalStore};
use localdb_core::{types::Span, ChunkRecord, StoreBackend};

use crate::tools::{AvailableStore, StoreDescriptor, StoresBackend};

/// Build a `StoreBackend` derived on demand from `stores` — see
/// `StoresBackend`'s doc comment. Lets `get_document` tests pass a real
/// `&dyn StoreBackend` to `tool_get_document` without maintaining a second,
/// parallel document registry alongside the `AvailableStore` fixtures they
/// already build.
pub(in crate::tools) fn backend_for(stores: &[AvailableStore]) -> Arc<dyn StoreBackend> {
    Arc::new(StoresBackend::new(stores))
}

pub(in crate::tools) fn make_descriptor(id: &str, name: &str) -> StoreDescriptor {
    StoreDescriptor {
        id: id.to_string(),
        name: name.to_string(),
        visibility: "private".to_string(),
    }
}

pub(in crate::tools) fn make_chunk(
    id: &str,
    resource_id: &str,
    store_id: &str,
    text: &str,
) -> ChunkRecord {
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
        modified_at: "2026-06-12T00:00:00Z".to_string(),
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
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    }
}

pub(in crate::tools) fn text_of(result: &CallToolResult) -> String {
    result.content[0].as_text().unwrap().text.clone()
}

/// Build two `AvailableStore`s that each hold a chunk for the *same*
/// `doc_id`, with distinguishable text, so a caller can tell which
/// store's copy a lookup returned.
pub(in crate::tools) async fn duplicate_doc_stores(
    doc_id: &str,
) -> (AvailableStore, AvailableStore) {
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
