//! E2E self-heal proof for issue #251's date extraction.
//!
//! Runs a real corpus fixture through the actual parser chain
//! (`extract::registry::build_chain` + `FileIngestor`) to get a genuine
//! `Resource` carrying `dc.date`/`date_source`. A second copy of that same
//! resource with the date fields stripped stands in for "a row indexed
//! before issue #251's date extraction existed" — same bytes, same
//! `content_hash`, same `policy_version`, only the newly-added metadata
//! differs. Re-indexing with the real (dated) resource must self-heal via
//! the metadata-only-update path (#176): `docs_metadata_updated == 1`,
//! `docs_indexed == 0`, and zero chunk/embedding writes — see
//! `core/tests/metadata_skip.rs`, whose `RecordingStore` harness this test
//! mirrors (that file is `core`-only and can't reach `extract`'s real
//! parsers, hence a separate copy here rather than a shared helper).

use std::sync::Mutex;

use async_trait::async_trait;
use extract::registry::{build_chain, default_parser_ids};
use ingest::FileIngestor;
use localdb_core::block::{IngestorKind, Resource};
use localdb_core::embedder::FakeEmbedder;
use localdb_core::ingestion::{
    run_source_ingestion, DeletionPolicy, DocumentIndex, DocumentRecord, FetchMetadata,
    IngestionConfig, SourceIngestionDeps,
};
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor, SkipReason};
use localdb_core::store::FakeStore;
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::uri::Uri;
use localdb_core::{
    ChunkRecord, ChunkerConfig, Error, MetadataFilter, ResourceRecord, RetrievalStore,
    SearchResult, StoreStats,
};

/// Captures every `Resource` a real `Ingestor::ingest` call produces.
#[derive(Default)]
struct CapturingCallback {
    resources: Vec<Resource>,
}

#[async_trait]
impl IngestCallback for CapturingCallback {
    async fn on_resource(&mut self, resource: Resource) -> Result<(), Error> {
        self.resources.push(resource);
        Ok(())
    }
    async fn on_discovered(&mut self, _total: usize) {}
    async fn on_skipped(&mut self, _uri: &Uri, _reason: SkipReason) {}
    async fn on_gone(&mut self, _uri: &Uri) {}
}

/// Run the real `FileIngestor` (real parser chain, issue #251's date
/// extraction included) over one temp-directory file and return its single
/// produced `Resource`.
async fn extract_real_resource(dir: &std::path::Path, filename: &str, bytes: &[u8]) -> Resource {
    std::fs::write(dir.join(filename), bytes).unwrap();

    let chain = build_chain(&default_parser_ids()).unwrap();
    let ingestor = FileIngestor::new(Box::new(chain));
    let source = IngestSource {
        source_id: "source-1".to_string(),
        store_id: "store-1".to_string(),
        ingestor_kind: IngestorKind::File,
        config: serde_json::json!({ "root": dir.to_string_lossy() }),
        policy_version: "policy-v1".to_string(),
        document_validators: FetchMetadata::default(),
    };
    let mut callback = CapturingCallback::default();
    ingestor.ingest(&source, &mut callback).await.unwrap();

    assert_eq!(
        callback.resources.len(),
        1,
        "expected exactly one resource from the fixture directory"
    );
    callback.resources.into_iter().next().unwrap()
}

/// Scripted `Ingestor`: yields exactly the resources it's given, in order
/// (mirrors `core/tests/metadata_skip.rs`'s `ScriptedIngestor`).
struct ScriptedIngestor {
    resources: Mutex<Vec<Resource>>,
}

impl ScriptedIngestor {
    fn new(resources: Vec<Resource>) -> Self {
        Self {
            resources: Mutex::new(resources),
        }
    }
}

#[async_trait]
impl Ingestor for ScriptedIngestor {
    fn kind(&self) -> IngestorKind {
        IngestorKind::File
    }

    async fn ingest(
        &self,
        _source: &IngestSource,
        callback: &mut dyn IngestCallback,
    ) -> Result<IngestResult, Error> {
        let resources: Vec<Resource> = std::mem::take(&mut *self.resources.lock().unwrap());
        callback.on_discovered(resources.len()).await;
        let mut produced = 0;
        for r in resources {
            callback.on_resource(r).await?;
            produced += 1;
        }
        Ok(IngestResult {
            resources_produced: produced,
            resources_skipped: 0,
            errors: 0,
            enumeration: Default::default(),
            document_validators: None,
        })
    }
}

/// Wraps `FakeStore`, recording every `update_resource_metadata` call —
/// mirrors `core/tests/metadata_skip.rs`'s `RecordingStore` (unreachable
/// from here: it's `core`-crate-private).
struct RecordingStore {
    inner: FakeStore,
    metadata_update_calls: Mutex<Vec<(String, String)>>,
}

impl RecordingStore {
    fn new() -> Self {
        Self {
            inner: FakeStore::new(),
            metadata_update_calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl RetrievalStore for RecordingStore {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        self.inner.upsert_chunks(records).await
    }

    async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
        self.inner.delete_by_resource(resource_id).await
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        self.inner.delete_by_store(store_id).await
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        self.inner.dense_search(query_vector, limit, filters).await
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        self.inner.bm25_search(query_text, limit, filters).await
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        self.inner.stats().await
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        self.inner.get_chunk(chunk_id).await
    }

    async fn get_chunks_for_resource(&self, resource_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        self.inner.get_chunks_for_resource(resource_id).await
    }

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        self.inner.list_indexed_documents().await
    }

    async fn update_resource_metadata(
        &self,
        store_id: &str,
        resource_id: &str,
        record: &ResourceRecord,
    ) -> Result<(), Error> {
        self.metadata_update_calls
            .lock()
            .unwrap()
            .push((store_id.to_string(), resource_id.to_string()));
        self.inner
            .update_resource_metadata(store_id, resource_id, record)
            .await
    }

    async fn get_resource_record(
        &self,
        store_id: &str,
        resource_id: &str,
    ) -> Result<Option<ResourceRecord>, Error> {
        self.inner.get_resource_record(store_id, resource_id).await
    }

    async fn upsert_blocks(
        &self,
        store_id: &str,
        resource_id: &str,
        blocks: &[localdb_core::block::Block],
    ) -> Result<(), Error> {
        self.inner
            .upsert_blocks(store_id, resource_id, blocks)
            .await
    }

    async fn get_blocks_for_resource(
        &self,
        resource_id: &str,
    ) -> Result<Vec<localdb_core::block::Block>, Error> {
        self.inner.get_blocks_for_resource(resource_id).await
    }
}

#[tokio::test]
async fn reindex_with_new_date_extraction_self_heals_as_metadata_only_update() {
    let dir = tempfile::tempdir().unwrap();
    // The corpus fixture with a bare front-matter date — exercises issue
    // #251's markdown front-matter extraction end-to-end.
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../extract/tests/fixtures/metadata/md-frontmatter-date.md"
    ))
    .expect("corpus fixture must exist");

    let resource_with_date = extract_real_resource(dir.path(), "post.md", &fixture).await;
    assert_eq!(
        resource_with_date.metadata.dublin_core().date.as_deref(),
        Some("2020-11-05"),
        "sanity: the real parser chain must have found the front-matter date"
    );
    assert_eq!(
        resource_with_date
            .metadata
            .dublin_core()
            .date_source
            .as_deref(),
        Some("front-matter")
    );

    // Stand-in for "a row indexed before issue #251 existed": same bytes
    // (same content_hash/blocks), same policy_version, but no date/date_source
    // — exactly what this fixture's `Metadata` looked like pre-#251.
    let mut resource_without_date = resource_with_date.clone();
    resource_without_date.metadata.dublin_core_mut().date = None;
    resource_without_date.metadata.dublin_core_mut().date_source = None;
    assert_eq!(
        resource_without_date.content_hash, resource_with_date.content_hash,
        "sanity: stripping date fields must not change content_hash"
    );

    let store = RecordingStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = IngestionConfig {
        store_id: store_id.to_string(),
        policy_version: "policy-v1".to_string(),
        chunker: ChunkerConfig::prose(),
    };
    let source = Source {
        id: "source-1".to_string(),
        store_id: store_id.to_string(),
        kind: SourceKind::Path,
        spec: SourceSpec::Path {
            root: dir.path().to_string_lossy().to_string(),
            include: vec![],
            exclude: vec![],
        },
        source_preset: "prose".to_string(),
    };

    // Run 1: the "pre-#251" state — no date on the resource yet.
    let mut doc_index = DocumentIndex::new();
    let ingestor1 = ScriptedIngestor::new(vec![resource_without_date]);
    let deps1 = SourceIngestionDeps {
        doc_index: &mut doc_index,
        store: &store,
        embedder: &embedder,
        config: &config,
        progress: None,
        deletion: DeletionPolicy::Retain,
        document_validators: FetchMetadata::default(),
    };
    let result1 = run_source_ingestion(&source, &ingestor1, deps1)
        .await
        .unwrap();
    assert_eq!(result1.docs_indexed, 1);

    let uri = resource_with_date.uri.as_str().to_string();
    let resource_id_str = doc_index.get(&uri).unwrap().resource_id.clone();
    let chunks_before = store
        .get_chunks_for_resource(&resource_id_str)
        .await
        .unwrap();
    assert!(!chunks_before.is_empty());
    let chunk_count_before = store.stats().await.unwrap().chunk_count;

    // Run 2: "the new extraction is now active" — same bytes, but this time
    // the resource carries the date the real parser chain actually found.
    let ingestor2 = ScriptedIngestor::new(vec![resource_with_date]);
    let deps2 = SourceIngestionDeps {
        doc_index: &mut doc_index,
        store: &store,
        embedder: &embedder,
        config: &config,
        progress: None,
        deletion: DeletionPolicy::Retain,
        document_validators: FetchMetadata::default(),
    };
    let result2 = run_source_ingestion(&source, &ingestor2, deps2)
        .await
        .unwrap();

    assert_eq!(
        result2.docs_metadata_updated, 1,
        "the newly-discovered date must self-heal as a metadata-only update"
    );
    assert_eq!(result2.docs_indexed, 0, "must not be a full reindex");
    assert_eq!(result2.docs_skipped, 0, "must not be an unchanged-skip");
    assert_eq!(
        store.stats().await.unwrap().chunk_count,
        chunk_count_before,
        "zero chunk writes — the metadata-only path must not touch chunks"
    );
    let chunks_after = store
        .get_chunks_for_resource(&resource_id_str)
        .await
        .unwrap();
    let chunk_shape = |c: &ChunkRecord| (c.id.clone(), c.text.clone(), c.embedding.clone());
    assert_eq!(
        chunks_before.iter().map(chunk_shape).collect::<Vec<_>>(),
        chunks_after.iter().map(chunk_shape).collect::<Vec<_>>(),
        "chunk-level fields must be byte-identical — no re-chunking, no re-embedding"
    );
    assert_eq!(
        store.metadata_update_calls.lock().unwrap().len(),
        1,
        "update_resource_metadata must be called exactly once"
    );
}
