use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use super::*;
use crate::backend::{SourceRow, StoreBackendConfig, StoreRow, TableSize};
use crate::block::BlockKind;
use crate::metadata::Metadata;
use crate::store::{FakeStore, RetrievalStore};
use crate::types::Span;

// ---------------------------------------------------------------------------
// A minimal fake `StoreBackend`, modeled on `store::FakeStore`: only
// `find_document`, `list_documents`, and `retrieval_store` carry real
// behavior — everything else is an unreachable stub since this read model
// never touches store/source registry operations.
// ---------------------------------------------------------------------------

struct FakeBackend {
    /// doc_id -> every `DocumentInfo` sharing that id, across stores —
    /// lets tests set up the cross-store ambiguity case.
    documents: HashMap<String, Vec<DocumentInfo>>,
    stores: HashMap<String, Arc<dyn RetrievalStore>>,
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            documents: HashMap::new(),
            stores: HashMap::new(),
        }
    }

    fn with_document(mut self, info: DocumentInfo) -> Self {
        self.documents
            .entry(info.id.clone())
            .or_default()
            .push(info);
        self
    }

    fn with_store(mut self, store_id: &str, store: Arc<dyn RetrievalStore>) -> Self {
        self.stores.insert(store_id.to_string(), store);
        self
    }
}

#[async_trait]
impl StoreBackend for FakeBackend {
    async fn open(_config: StoreBackendConfig) -> Result<Self, Error> {
        unimplemented!("never constructed via the trait's own open()")
    }

    async fn upsert_store(&self, _store: &StoreRow) -> Result<(), Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn delete_store(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn get_store(&self, _id: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn get_store_by_name(&self, _name: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn upsert_source(&self, _source: &SourceRow) -> Result<(), Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn delete_source(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn get_source(&self, _id: &str) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn list_sources(&self, _store_id: &str) -> Result<Vec<SourceRow>, Error> {
        unimplemented!("not exercised by the document read model")
    }
    async fn find_source_by_root_or_url(
        &self,
        _value: &str,
        _store_id: &str,
    ) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised by the document read model")
    }

    async fn find_document(
        &self,
        doc_id: &str,
        store_id: Option<&str>,
    ) -> Result<Option<DocumentInfo>, Error> {
        let matches: Vec<&DocumentInfo> = self
            .documents
            .get(doc_id)
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        match store_id {
            Some(scope) => Ok(matches.into_iter().find(|d| d.store_id == scope).cloned()),
            None => match matches.len() {
                0 => Ok(None),
                1 => Ok(Some(matches[0].clone())),
                _ => Err(Error::InvalidRequest {
                    message: format!(
                        "document '{doc_id}' exists in multiple stores; use store-scoped search to disambiguate"
                    ),
                }),
            },
        }
    }

    async fn list_documents(
        &self,
        store_id: &str,
        source_id: Option<&str>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Vec<DocumentInfo>, Error> {
        let mut out: Vec<DocumentInfo> = self
            .documents
            .values()
            .flatten()
            .filter(|d| d.store_id == store_id)
            .filter(|d| source_id.map(|s| d.source_id == s).unwrap_or(true))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.uri.cmp(&b.uri));
        let paged = match limit {
            Some(limit) => out.into_iter().skip(offset).take(limit).collect(),
            None => out.into_iter().skip(offset).collect(),
        };
        Ok(paged)
    }

    async fn count_documents(&self, store_id: &str, source_id: Option<&str>) -> Result<u64, Error> {
        let count = self
            .documents
            .values()
            .flatten()
            .filter(|d| d.store_id == store_id)
            .filter(|d| source_id.map(|s| d.source_id == s).unwrap_or(true))
            .count();
        Ok(count as u64)
    }

    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        self.stores
            .get(store_id)
            .cloned()
            .ok_or_else(|| Error::StoreNotFound {
                id: store_id.to_string(),
            })
    }

    async fn largest_tables(&self, _limit: usize) -> Result<Vec<TableSize>, Error> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn make_document_info(id: &str, store_id: &str, uri: &str) -> DocumentInfo {
    DocumentInfo {
        store_id: store_id.to_string(),
        id: id.to_string(),
        source_id: "src-1".to_string(),
        ingestor_kind: "path".to_string(),
        uri: uri.to_string(),
        title: None,
        mime: None,
        content_hash: "hash".to_string(),
        fetched_at: "2026-01-01T00:00:00Z".to_string(),
        origin_store: store_id.to_string(),
        policy_version: "v1".to_string(),
        metadata: Metadata::default(),
        date_original: None,
        date_parsed: None,
        index_updated_at: None,
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
        embedding: vec![],
        policy_version: "v1".to_string(),
        fetched_at: "2026-01-01T00:00:00Z".to_string(),
        modified_at: "2026-01-01T00:00:00Z".to_string(),
        content_hash: "hash".to_string(),
        origin_store: store_id.to_string(),
        source_id: "src-1".to_string(),
        ingestor_kind: "path".to_string(),
        mime: None,
        uri: "file:///doc.md".to_string(),
        metadata: Metadata::default(),
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

fn make_block(seq: u32, text: &str) -> Block {
    Block {
        seq,
        kind: BlockKind::Text,
        text: text.to_string(),
        location: None,
    }
}

// ---------------------------------------------------------------------------
// get_document_detail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_document_detail_without_text_leaves_text_none() {
    let backend =
        FakeBackend::new().with_document(make_document_info("doc-1", "store-a", "file:///a.md"));

    let detail = get_document_detail(&backend, "doc-1", None, false)
        .await
        .unwrap();

    assert_eq!(detail.info.id, "doc-1");
    assert!(detail.text.is_none());
    assert_eq!(
        detail.chunk_count, None,
        "chunk_count must be None when include_text is false — chunks are never fetched"
    );
}

#[tokio::test]
async fn get_document_detail_with_text_populates_chunk_count() {
    let store = FakeStore::new();
    store
        .upsert_chunks(vec![
            make_chunk("chunk-1", "doc-1", "store-a", "chunk one"),
            make_chunk("chunk-2", "doc-1", "store-a", "chunk two"),
            make_chunk("chunk-3", "doc-1", "store-a", "chunk three"),
        ])
        .await
        .unwrap();

    let backend = FakeBackend::new()
        .with_document(make_document_info("doc-1", "store-a", "file:///a.md"))
        .with_store("store-a", Arc::new(store));

    let detail = get_document_detail(&backend, "doc-1", None, true)
        .await
        .unwrap();

    assert_eq!(
        detail.chunk_count,
        Some(3),
        "chunk_count must equal the number of chunks fetched to build text"
    );
}

#[tokio::test]
async fn get_document_detail_with_text_prefers_blocks_over_chunks() {
    let store = FakeStore::new();
    store
        .upsert_chunks(vec![
            make_chunk("chunk-1", "doc-1", "store-a", "chunk one"),
            make_chunk("chunk-2", "doc-1", "store-a", "chunk two"),
        ])
        .await
        .unwrap();
    store
        .upsert_blocks(
            "store-a",
            "doc-1",
            &[make_block(0, "first block"), make_block(1, "second block")],
        )
        .await
        .unwrap();

    let backend = FakeBackend::new()
        .with_document(make_document_info("doc-1", "store-a", "file:///a.md"))
        .with_store("store-a", Arc::new(store));

    let detail = get_document_detail(&backend, "doc-1", None, true)
        .await
        .unwrap();

    assert_eq!(
        detail.text.as_deref(),
        Some("first block\n\nsecond block"),
        "blocks must be used when present, joined with \\n\\n"
    );
}

#[tokio::test]
async fn get_document_detail_with_text_falls_back_to_chunks_when_no_blocks() {
    let store = FakeStore::new();
    store
        .upsert_chunks(vec![
            make_chunk("chunk-1", "doc-1", "store-a", "chunk one"),
            make_chunk("chunk-2", "doc-1", "store-a", "chunk two"),
        ])
        .await
        .unwrap();

    let backend = FakeBackend::new()
        .with_document(make_document_info("doc-1", "store-a", "file:///a.md"))
        .with_store("store-a", Arc::new(store));

    let detail = get_document_detail(&backend, "doc-1", None, true)
        .await
        .unwrap();

    assert_eq!(
        detail.text.as_deref(),
        Some("chunk one\nchunk two"),
        "chunk texts joined with \\n when no blocks were persisted"
    );
}

#[tokio::test]
async fn get_document_detail_store_scoped_disambiguates() {
    let backend = FakeBackend::new()
        .with_document(make_document_info("doc-shared", "store-a", "file:///a.md"))
        .with_document(make_document_info("doc-shared", "store-b", "file:///b.md"));

    let detail = get_document_detail(&backend, "doc-shared", Some("store-b"), false)
        .await
        .unwrap();

    assert_eq!(detail.info.store_id, "store-b");
}

#[tokio::test]
async fn get_document_detail_unscoped_ambiguity_errors() {
    let backend = FakeBackend::new()
        .with_document(make_document_info("doc-shared", "store-a", "file:///a.md"))
        .with_document(make_document_info("doc-shared", "store-b", "file:///b.md"));

    let err = get_document_detail(&backend, "doc-shared", None, false)
        .await
        .unwrap_err();

    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn get_document_detail_not_found_propagates() {
    let backend = FakeBackend::new();

    let err = get_document_detail(&backend, "missing", None, false)
        .await
        .unwrap_err();

    assert_eq!(
        err,
        Error::ResourceNotFound {
            id: "missing".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// get_document_detail_scoped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scoped_empty_allowlist_is_unscoped_and_keeps_ambiguity_error() {
    let backend = FakeBackend::new()
        .with_document(make_document_info("doc-shared", "store-a", "file:///a.md"))
        .with_document(make_document_info("doc-shared", "store-b", "file:///b.md"));

    let err = get_document_detail_scoped(&backend, "doc-shared", &[], false)
        .await
        .unwrap_err();

    assert!(
        matches!(err, Error::InvalidRequest { .. }),
        "an empty allowlist must behave exactly like an unscoped find_document call"
    );
}

#[tokio::test]
async fn scoped_single_id_is_sql_scoped_even_when_ambiguous_elsewhere() {
    let backend = FakeBackend::new()
        .with_document(make_document_info("doc-shared", "store-a", "file:///a.md"))
        .with_document(make_document_info("doc-shared", "store-b", "file:///b.md"));

    let detail =
        get_document_detail_scoped(&backend, "doc-shared", &["store-a".to_string()], false)
            .await
            .unwrap();

    assert_eq!(
        detail.info.store_id, "store-a",
        "a single allowed store must resolve via the SQL-scoped path, never hitting the \
         cross-store ambiguity error"
    );
}

#[tokio::test]
async fn scoped_many_accepts_a_document_inside_the_allowed_set() {
    let backend =
        FakeBackend::new().with_document(make_document_info("doc-1", "store-a", "file:///a.md"));

    let detail = get_document_detail_scoped(
        &backend,
        "doc-1",
        &["store-a".to_string(), "store-b".to_string()],
        false,
    )
    .await
    .unwrap();

    assert_eq!(detail.info.store_id, "store-a");
}

#[tokio::test]
async fn scoped_many_rejects_a_document_outside_the_allowed_set() {
    let backend =
        FakeBackend::new().with_document(make_document_info("doc-1", "store-c", "file:///c.md"));

    let err = get_document_detail_scoped(
        &backend,
        "doc-1",
        &["store-a".to_string(), "store-b".to_string()],
        false,
    )
    .await
    .unwrap_err();

    assert_eq!(
        err,
        Error::ResourceNotFound {
            id: "doc-1".to_string()
        },
        "a document outside the caller's visible stores must read as not-found, not leak \
         its existence"
    );
}

#[tokio::test]
async fn scoped_not_found_propagates_for_every_arity() {
    let backend = FakeBackend::new();

    for allowed in [
        Vec::<String>::new(),
        vec!["store-a".to_string()],
        vec!["store-a".to_string(), "store-b".to_string()],
    ] {
        let err = get_document_detail_scoped(&backend, "missing", &allowed, false)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            Error::ResourceNotFound {
                id: "missing".to_string()
            },
            "not-found must propagate regardless of allowlist arity (len={})",
            allowed.len()
        );
    }
}

// ---------------------------------------------------------------------------
// reconstruct_document_text
// ---------------------------------------------------------------------------

#[test]
fn reconstruct_document_text_empty_chunks_and_blocks_is_empty_string() {
    assert_eq!(reconstruct_document_text(&[], &[]), "");
}

#[test]
fn reconstruct_document_text_chunks_only_joins_with_newline() {
    let chunks = vec![
        make_chunk("c1", "doc-1", "store-a", "alpha"),
        make_chunk("c2", "doc-1", "store-a", "beta"),
    ];
    assert_eq!(reconstruct_document_text(&chunks, &[]), "alpha\nbeta");
}

#[test]
fn reconstruct_document_text_blocks_present_ignores_chunks() {
    let chunks = vec![make_chunk("c1", "doc-1", "store-a", "chunk text")];
    let blocks = vec![make_block(0, "block one"), make_block(1, "block two")];
    assert_eq!(
        reconstruct_document_text(&chunks, &blocks),
        "block one\n\nblock two"
    );
}
