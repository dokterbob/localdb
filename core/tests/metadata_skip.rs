//! Metadata-only incremental update (issue #176; specs/04-search-pipeline.md).
//!
//! Compare = `content_hash` + `policy_version` + `metadata_hash` (the hash of
//! post-backfill persisted metadata state — see
//! `core::ids::compute_metadata_hash`'s doc comment). A metadata_hash
//! mismatch with `content_hash`/`policy_version` unchanged is a metadata-only
//! write: no chunk/embedding writes, `index_updated_at` bumps, `added_at`
//! untouched. See `core/src/ingestion.rs`'s `PipelineCallback::on_resource`.
//!
//! Lives as a sibling integration test file (not inline in
//! `core/src/ingestion.rs`, which already carries its own large `mod tests`)
//! per repo convention for new test additions to that file.

use std::sync::Mutex;

use async_trait::async_trait;
use localdb_core::block::{Block, BlockKind, IngestorKind, Resource, ResourceKind};
use localdb_core::embedder::FakeEmbedder;
use localdb_core::ids::{content_hash, new_ulid, resource_id};
use localdb_core::ingestion::{
    index_resource, run_source_ingestion, DeletionPolicy, DocumentIndex, DocumentRecord,
    IndexOutcome, IndexResourceDeps, IngestionConfig, SourceIngestionDeps,
};
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor};
use localdb_core::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};
use localdb_core::progress::DocOutcome;
use localdb_core::store::FakeStore;
use localdb_core::types::{Source, SourceKind, SourceSpec};
use localdb_core::uri::Uri;
use localdb_core::{
    ChunkRecord, ChunkerConfig, Error, MetadataFilter, ResourceRecord, RetrievalStore,
    SearchResult, StoreStats,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn make_source_with_preset(store_id: &str, preset: &str) -> Source {
    Source {
        id: new_ulid(),
        store_id: store_id.to_string(),
        kind: SourceKind::Path,
        spec: SourceSpec::Path {
            root: "/docs".to_string(),
            include: vec![],
            exclude: vec![],
        },
        source_preset: preset.to_string(),
    }
}

fn make_ingestion_config(store_id: &str) -> IngestionConfig {
    IngestionConfig {
        store_id: store_id.to_string(),
        policy_version: "policy-v1".to_string(),
        chunker: ChunkerConfig::prose(),
    }
}

/// Full-control resource builder: every field the metadata-only-update
/// contract cares about (`title`/Dublin Core title backfill,
/// `external_id`/`external_etag`, `modified_at`) is a parameter.
#[allow(clippy::too_many_arguments)]
fn make_resource_full(
    uri: &str,
    text: &str,
    source_id: &str,
    store_id: &str,
    title: Option<&str>,
    metadata_title: Option<&str>,
    external_id: Option<&str>,
    external_etag: Option<&str>,
    modified_at: &str,
) -> Resource {
    let hash = content_hash(text);
    let id = resource_id(uri, &hash);
    Resource {
        id,
        store_id: store_id.to_string(),
        source_id: source_id.to_string(),
        ingestor_kind: IngestorKind::File,
        resource_kind: ResourceKind::Document,
        uri: Uri::parse(uri).unwrap_or_else(|| panic!("invalid test uri: {uri}")),
        external_id: external_id.map(str::to_string),
        external_etag: external_etag.map(str::to_string),
        content_hash: hash,
        title: title.map(str::to_string),
        mime: Some("text/markdown".to_string()),
        metadata: Metadata::Document(DocumentMetadata {
            dublin_core: DublinCoreMetadata {
                title: metadata_title.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        }),
        added_at: "2026-06-10T12:00:00Z".to_string(),
        modified_at: modified_at.to_string(),
        thread_id: None,
        channel: None,
        participants: vec![],
        origin_store: store_id.to_string(),
        policy_version: "policy-v1".to_string(),
        share_path: None,
        extractor_version: "1".to_string(),
        blocks: vec![Block {
            seq: 0,
            kind: BlockKind::Text,
            text: text.to_string(),
            location: None,
        }],
    }
}

/// Plain resource with no title, no metadata title, no external fields — the
/// baseline every test starts from before varying one field.
fn make_resource(uri: &str, text: &str, source_id: &str, store_id: &str) -> Resource {
    make_resource_full(
        uri,
        text,
        source_id,
        store_id,
        None,
        None,
        None,
        None,
        "2026-06-10T12:00:00Z",
    )
}

/// Scripted `Ingestor`: yields exactly the resources it's given, in order.
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
        })
    }
}

/// Wraps `FakeStore`, recording every `update_resource_metadata` call — the
/// counter the metadata-only-update tests assert on (mirrors
/// `core::ingestion`'s own private `RecordingStore`, unusable here since
/// it's `mod tests`-private to that file).
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

    fn metadata_update_calls(&self) -> Vec<(String, String)> {
        self.metadata_update_calls.lock().unwrap().clone()
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

async fn run_once(
    store: &RecordingStore,
    embedder: &FakeEmbedder,
    config: &IngestionConfig,
    source: &Source,
    doc_index: &mut DocumentIndex,
    resources: Vec<Resource>,
    deletion: DeletionPolicy,
) -> localdb_core::ingestion::IngestionResult {
    let ingestor = ScriptedIngestor::new(resources);
    let deps = SourceIngestionDeps {
        doc_index,
        store,
        embedder,
        config,
        progress: None,
        deletion,
    };
    run_source_ingestion(source, &ingestor, deps).await.unwrap()
}

// ---------------------------------------------------------------------------
// 1. on_resource_metadata_only_change_writes_no_chunks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_resource_metadata_only_change_writes_no_chunks() {
    let store = RecordingStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = make_ingestion_config(store_id);
    let source = make_source_with_preset(store_id, "prose");
    let uri = "file:///docs/meta.md";
    let text = "Content that never changes across these two runs.";

    let mut doc_index = DocumentIndex::new();
    let first = make_resource(uri, text, &source.id, store_id);
    let result1 = run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![first],
        DeletionPolicy::Retain,
    )
    .await;
    assert_eq!(result1.docs_indexed, 1);

    let resource_id_str = doc_index.get(uri).unwrap().resource_id.clone();
    let chunks_before = store
        .get_chunks_for_resource(&resource_id_str)
        .await
        .unwrap();
    assert!(!chunks_before.is_empty());

    // Same content/policy, but external_id now set — a metadata-only change.
    let second = make_resource_full(
        uri,
        text,
        &source.id,
        store_id,
        None,
        None,
        Some("urn:entry:1"),
        None,
        "2026-06-10T12:00:00Z",
    );
    let result2 = run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![second],
        DeletionPolicy::Retain,
    )
    .await;

    assert_eq!(result2.docs_indexed, 0, "no full reindex");
    assert_eq!(result2.docs_skipped, 0, "not an unchanged-skip either");
    assert_eq!(result2.docs_metadata_updated, 1);

    let chunks_after = store
        .get_chunks_for_resource(&resource_id_str)
        .await
        .unwrap();
    // Compare only the genuinely chunk-level fields — id/text/span/embedding/
    // block_seq/etc. `FakeStore` has no separate `resources` table, so it
    // denormalizes resource-level fields (metadata, external_id, ...) onto
    // each `ChunkRecord`; those are EXPECTED to change here (that's what
    // `update_resource_metadata` just wrote), and comparing the whole struct
    // would fail on exactly the fields the update was supposed to touch. The
    // real backend's `chunks` table has no such columns at all — proven
    // separately by
    // `store-libsql`'s `metadata_only_update_does_not_touch_chunks_embeddings_or_fts`.
    let chunk_shape = |c: &ChunkRecord| {
        (
            c.id.clone(),
            c.text.clone(),
            c.span.clone(),
            c.embedding.clone(),
            c.block_seq,
            c.seq_in_block,
            c.block_kind.clone(),
            c.page,
            c.window_block_seqs.clone(),
            c.content_hash.clone(),
            c.policy_version.clone(),
        )
    };
    assert_eq!(
        chunks_before.iter().map(chunk_shape).collect::<Vec<_>>(),
        chunks_after.iter().map(chunk_shape).collect::<Vec<_>>(),
        "chunk-level fields (text/span/embedding/block position) must be byte-identical \
         before/after a metadata-only update — no re-chunking, no re-embedding"
    );
    assert_ne!(
        chunks_before[0].external_id, chunks_after[0].external_id,
        "sanity: the metadata-only update did change what it was supposed to change"
    );
    assert_eq!(
        store.metadata_update_calls(),
        vec![(store_id.to_string(), resource_id_str)],
        "update_resource_metadata must be called exactly once"
    );
}

// ---------------------------------------------------------------------------
// 2. on_resource_unchanged_metadata_still_zero_writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_resource_unchanged_metadata_still_zero_writes() {
    let store = RecordingStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = make_ingestion_config(store_id);
    let source = make_source_with_preset(store_id, "prose");
    let uri = "file:///docs/stable.md";
    let text = "Stable content that never changes.";

    let mut doc_index = DocumentIndex::new();
    let first = make_resource(uri, text, &source.id, store_id);
    run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![first],
        DeletionPolicy::Retain,
    )
    .await;

    let chunk_count_before = store.stats().await.unwrap().chunk_count;

    // Identical resource, second run.
    let second = make_resource(uri, text, &source.id, store_id);
    let result2 = run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![second],
        DeletionPolicy::Retain,
    )
    .await;

    assert_eq!(result2.docs_indexed, 0);
    assert_eq!(result2.docs_skipped, 1);
    assert_eq!(result2.docs_metadata_updated, 0);
    assert_eq!(store.stats().await.unwrap().chunk_count, chunk_count_before);
    assert!(
        store.metadata_update_calls().is_empty(),
        "update_resource_metadata must not be called when nothing changed"
    );
}

// ---------------------------------------------------------------------------
// 3. on_resource_content_change_still_does_full_reindex (regression)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_resource_content_change_still_does_full_reindex() {
    let store = RecordingStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = make_ingestion_config(store_id);
    let source = make_source_with_preset(store_id, "prose");
    let uri = "file:///docs/changing.md";

    let mut doc_index = DocumentIndex::new();
    let first = make_resource(uri, "Original content.", &source.id, store_id);
    run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![first],
        DeletionPolicy::Retain,
    )
    .await;

    let second = make_resource(
        uri,
        "Completely different content now.",
        &source.id,
        store_id,
    );
    let result2 = run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![second],
        DeletionPolicy::Retain,
    )
    .await;

    assert_eq!(
        result2.docs_indexed, 1,
        "content change forces a full reindex"
    );
    assert_eq!(result2.docs_metadata_updated, 0);
    assert!(
        store.metadata_update_calls().is_empty(),
        "a full reindex must go through upsert_chunks_and_blocks, not update_resource_metadata"
    );
}

// ---------------------------------------------------------------------------
// 4. list_indexed_documents_metadata_hash_matches_index_resource_stamped_hash
//    (the backfilled-title trap)
// ---------------------------------------------------------------------------

/// The trap this whole design hinges on closing: a resource whose title was
/// BACKFILLED (`resource.title` carries it, `resource.metadata`'s own Dublin
/// Core title does not) must produce the SAME `metadata_hash` whether it's
/// read at stamp-time (`index_resource`'s return value) or at
/// rehydration-time (`list_indexed_documents`, reading back the persisted
/// `metadata_json`/`external_id`/`external_etag`/`modified_at` columns).
#[tokio::test]
async fn list_indexed_documents_metadata_hash_matches_index_resource_stamped_hash() {
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = make_ingestion_config(store_id);
    let source = make_source_with_preset(store_id, "prose");

    // title Some, metadata dublin_core title None — backfill fires.
    let resource = make_resource_full(
        "file:///docs/backfilled.md",
        "Some content whose title comes from extraction, not front-matter.",
        &source.id,
        store_id,
        Some("Backfilled Title"),
        None,
        Some("urn:entry:42"),
        Some("\"etag-42\""),
        "2026-06-15T00:00:00Z",
    );

    let deps = IndexResourceDeps {
        store: &store,
        embedder: &embedder,
        config: &config,
    };
    let outcome = index_resource(&resource, &source, None, &deps)
        .await
        .unwrap();
    let stamped_hash = match outcome {
        IndexOutcome::Written(_, hash) => hash,
        IndexOutcome::Empty => panic!("resource must not chunk to empty"),
    };

    let records = RetrievalStore::list_indexed_documents(&store)
        .await
        .unwrap();
    let rehydrated = records
        .into_iter()
        .find(|r| r.resource_id == resource.id)
        .expect("the indexed resource must appear in list_indexed_documents");

    assert_eq!(
        rehydrated.metadata_hash, stamped_hash,
        "stamp-time and rehydration-time hashes must agree for a backfilled-title resource"
    );
}

// ---------------------------------------------------------------------------
// 5. rehydrated_index_detects_metadata_only_change_across_restart
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rehydrated_index_detects_metadata_only_change_across_restart() {
    let store = FakeStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = make_ingestion_config(store_id);
    let source = make_source_with_preset(store_id, "prose");
    let uri = "file:///docs/rehydrate.md";
    let text = "Content that survives a simulated process restart.";

    // First "process": full index via the scripted ingestor.
    let mut doc_index1 = DocumentIndex::new();
    let first = make_resource(uri, text, &source.id, store_id);
    let ingestor1 = ScriptedIngestor::new(vec![first]);
    let deps1 = SourceIngestionDeps {
        doc_index: &mut doc_index1,
        store: &store,
        embedder: &embedder,
        config: &config,
        progress: None,
        deletion: DeletionPolicy::Retain,
    };
    let result1 = run_source_ingestion(&source, &ingestor1, deps1)
        .await
        .unwrap();
    assert_eq!(result1.docs_indexed, 1);

    // Simulate a new process: rehydrate DocumentIndex from the store.
    let records = RetrievalStore::list_indexed_documents(&store)
        .await
        .unwrap();
    let mut doc_index2 = DocumentIndex::from_records(records);

    // Second "process": same content, but a new external_etag (e.g. the
    // origin re-served the same bytes with a bumped change-detection token).
    let second = make_resource_full(
        uri,
        text,
        &source.id,
        store_id,
        None,
        None,
        None,
        Some("\"etag-new\""),
        "2026-06-10T12:00:00Z",
    );
    let ingestor2 = ScriptedIngestor::new(vec![second]);
    let deps2 = SourceIngestionDeps {
        doc_index: &mut doc_index2,
        store: &store,
        embedder: &embedder,
        config: &config,
        progress: None,
        deletion: DeletionPolicy::Retain,
    };
    let result2 = run_source_ingestion(&source, &ingestor2, deps2)
        .await
        .unwrap();

    assert_eq!(
        result2.docs_metadata_updated, 1,
        "a metadata-only change must be detected across a rehydrated DocumentIndex"
    );
    assert_eq!(result2.docs_indexed, 0);
    assert_eq!(result2.docs_skipped, 0);
}

// ---------------------------------------------------------------------------
// 6. metadata_updated_uri_survives_delete_sweep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn metadata_updated_uri_survives_delete_sweep() {
    let store = RecordingStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = make_ingestion_config(store_id);
    let source = make_source_with_preset(store_id, "prose");
    let uri = "file:///docs/survivor.md";
    let text = "Content whose metadata will change but URI must survive the sweep.";

    let mut doc_index = DocumentIndex::new();
    let first = make_resource(uri, text, &source.id, store_id);
    run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![first],
        DeletionPolicy::Prune,
    )
    .await;

    let resource_id_str = doc_index.get(uri).unwrap().resource_id.clone();

    // Metadata-only change, run under Prune — the delete-sweep must not
    // remove this URI: it's in `seen` (inserted at the top of on_resource),
    // exactly like an ordinary skip or a full reindex.
    let second = make_resource_full(
        uri,
        text,
        &source.id,
        store_id,
        None,
        None,
        Some("urn:entry:survivor"),
        None,
        "2026-06-10T12:00:00Z",
    );
    let result2 = run_once(
        &store,
        &embedder,
        &config,
        &source,
        &mut doc_index,
        vec![second],
        DeletionPolicy::Prune,
    )
    .await;

    assert_eq!(result2.docs_metadata_updated, 1);
    assert_eq!(
        result2.docs_deleted, 0,
        "the sweep must not delete the metadata-updated URI"
    );
    assert!(
        doc_index.get(uri).is_some(),
        "the URI must still be present in doc_index after the sweep"
    );
    let chunks = store
        .get_chunks_for_resource(&resource_id_str)
        .await
        .unwrap();
    assert!(
        !chunks.is_empty(),
        "the resource's chunks must still be present after the sweep"
    );
}

// ---------------------------------------------------------------------------
// 7. update_resource_metadata zero-rows behavior (FakeStore side)
// ---------------------------------------------------------------------------

/// `FakeStore::update_resource_metadata` mirrors `TenantStore`'s zero-rows
/// semantics: `Err(Error::ResourceNotFound)`, never a silent `Ok(())`.
#[tokio::test]
async fn fake_store_update_resource_metadata_errors_on_unknown_resource() {
    let store = FakeStore::new();
    let record = ResourceRecord {
        metadata: Metadata::default(),
        external_id: None,
        external_etag: None,
        modified_at: "2026-06-10T12:00:00Z".to_string(),
        date_original: None,
        date_parsed: None,
    };
    let err = store
        .update_resource_metadata("store-1", "does-not-exist", &record)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::ResourceNotFound {
            id: "does-not-exist".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// 8. Existing skip-invariant regressions, exercised through this file's own
//    fixtures (the ones enumerated in core/src/ingestion.rs's own `mod
//    tests` are covered there and are NOT duplicated here — this just adds
//    the `DocOutcome::MetadataUpdated` progress-event check the new arm
//    introduces).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn metadata_only_update_emits_metadata_updated_progress_event() {
    let store = RecordingStore::new();
    let embedder = FakeEmbedder::new(4);
    let store_id = "store-1";
    let config = make_ingestion_config(store_id);
    let source = make_source_with_preset(store_id, "prose");
    let uri = "file:///docs/progress.md";
    let text = "Content for progress-event assertions.";

    let mut doc_index = DocumentIndex::new();
    let first = make_resource(uri, text, &source.id, store_id);
    let ingestor1 = ScriptedIngestor::new(vec![first]);
    let deps1 = SourceIngestionDeps {
        doc_index: &mut doc_index,
        store: &store,
        embedder: &embedder,
        config: &config,
        progress: None,
        deletion: DeletionPolicy::Retain,
    };
    run_source_ingestion(&source, &ingestor1, deps1)
        .await
        .unwrap();

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let events2 = events.clone();
    let sink: localdb_core::progress::ProgressSink =
        std::sync::Arc::new(move |e| events2.lock().unwrap().push(e));

    let second = make_resource_full(
        uri,
        text,
        &source.id,
        store_id,
        None,
        None,
        Some("urn:entry:progress"),
        None,
        "2026-06-10T12:00:00Z",
    );
    let ingestor2 = ScriptedIngestor::new(vec![second]);
    let deps2 = SourceIngestionDeps {
        doc_index: &mut doc_index,
        store: &store,
        embedder: &embedder,
        config: &config,
        progress: Some(sink),
        deletion: DeletionPolicy::Retain,
    };
    run_source_ingestion(&source, &ingestor2, deps2)
        .await
        .unwrap();

    let events = events.lock().unwrap();
    let found = events.iter().any(|e| {
        matches!(
            e,
            localdb_core::progress::ProgressEvent::DocumentFinished {
                outcome: DocOutcome::MetadataUpdated,
                ..
            }
        )
    });
    assert!(
        found,
        "a metadata-only update must emit DocOutcome::MetadataUpdated, got: {events:?}"
    );
}
