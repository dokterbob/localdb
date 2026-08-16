use axum::{
    routing::{delete, get, post},
    Router,
};
use serde_json::json;
use tempfile::TempDir;

use crate::handlers::{
    cancel_job, create_job, create_source, create_store, delete_source, delete_store, get_config,
    get_document, get_job, get_status, get_store, job_events, list_sources, list_stores,
    patch_store, search,
};
use crate::state::AppState;

pub(crate) async fn make_app() -> (TempDir, Router) {
    make_app_with_queue(crate::job_queue::JobQueue::new()).await
}

/// Like [`make_app`], but with a caller-supplied `JobQueue` instead of a
/// fresh `JobQueue::new()` — lets a test build the app around a queue
/// constructed via `JobQueue::new_with_event_capacity` (issue #187 review,
/// finding 4d), so it can force `RecvError::Lagged` on the real `GET
/// /v1/jobs/{id}/events` route without needing 1024+ real progress events.
pub(crate) async fn make_app_with_queue(queue: crate::job_queue::JobQueue) -> (TempDir, Router) {
    let (dir, router, _state) = make_app_with_queue_and_state(queue).await;
    (dir, router)
}

/// Like [`make_app_with_queue`], but also returns the `AppState` the router
/// was built around (cloned before `with_state` consumes it — `AppState` is
/// `Arc`-backed, so the clone shares the same `Inner`). Lets a test observe
/// state that isn't reachable through any HTTP route, e.g.
/// `AppState::embedder_build_count` (Codex review finding F2, issue #187).
pub(crate) async fn make_app_with_queue_and_state(
    queue: crate::job_queue::JobQueue,
) -> (TempDir, Router, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let yaml_config = localdb_core::config::schema::RawConfig {
        defaults: localdb_core::config::schema::DefaultsConfig {
            indexing: localdb_core::config::schema::IndexingPolicyConfig {
                chunking: Default::default(),
                embedding: localdb_core::config::schema::EmbeddingPolicy {
                    provider: "fake".to_string(),
                    model: "default".to_string(),
                },
                ..Default::default()
            },
        },
        ..Default::default()
    };
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
    )
    .await
    .unwrap();

    let router = build_router(state.clone());

    (dir, router, state)
}

/// Build the router around an arbitrary `AppState` — factored out of
/// [`make_app_with_queue_and_state`] so a test that needs a non-standard
/// backend (e.g. one that fails or tracks calls for a specific store) can
/// assemble its own `AppState` via `AppState::from_backend` and still get
/// the real route table rather than a route subset.
pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/stores", get(list_stores).post(create_store))
        .route(
            "/v1/stores/{name}",
            get(get_store).patch(patch_store).delete(delete_store),
        )
        .route(
            "/v1/stores/{name}/sources",
            get(list_sources).post(create_source),
        )
        .route("/v1/sources/{id}", delete(delete_source))
        .route("/v1/documents/{id}", get(get_document))
        .route("/v1/search", post(search))
        .route("/v1/jobs", post(create_job))
        .route("/v1/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/v1/jobs/{id}/events", get(job_events))
        .route("/v1/status", get(get_status))
        .route("/v1/config", get(get_config))
        .with_state(state)
}

pub(crate) async fn json_body(body: axum::body::Body) -> serde_json::Value {
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

pub(crate) async fn make_state_with_fake_config() -> (TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let yaml_config = localdb_core::config::schema::RawConfig {
        defaults: localdb_core::config::schema::DefaultsConfig {
            indexing: localdb_core::config::schema::IndexingPolicyConfig {
                chunking: Default::default(),
                embedding: localdb_core::config::schema::EmbeddingPolicy {
                    provider: "fake".to_string(),
                    model: "default".to_string(),
                },
                ..Default::default()
            },
        },
        ..Default::default()
    };
    let queue = crate::job_queue::JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().join("models"),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
    )
    .await
    .unwrap();
    (dir, state)
}

pub(crate) struct SeedChunkInput {
    pub(crate) chunk_id: &'static str,
    pub(crate) doc_id: &'static str,
    pub(crate) text: &'static str,
    pub(crate) uri: &'static str,
    pub(crate) metadata: localdb_core::metadata::Metadata,
}

pub(crate) async fn seed_store_a_chunk(state: &AppState, input: SeedChunkInput) {
    use localdb_core::Embedder;

    state.add_store("store-A", "private").await.unwrap();
    let source = state
        .add_source("store-A", "path", json!({"root": "/tmp"}), "prose", None)
        .await
        .unwrap();
    let store_id = source.store_id.clone();
    let embedder = localdb_core::FakeEmbedder::new(128);
    let docs = vec![localdb_core::embedder::DocumentChunks {
        document_context: input.text.to_string(),
        chunks: vec![input.text.to_string()],
    }];
    let embedding = embedder
        .embed_documents(docs)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let chunk = localdb_core::ChunkRecord {
        id: input.chunk_id.to_string(),
        resource_id: input.doc_id.to_string(),
        store_id: store_id.clone(),
        text: input.text.to_string(),
        span: localdb_core::types::Span::new(0, input.text.len()),
        heading_path: vec![],
        embedding,
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: "abc123".to_string(),
        origin_store: store_id.clone(),
        source_id: source.id,
        ingestor_kind: "path".to_string(),
        mime: Some("text/plain".to_string()),
        uri: input.uri.to_string(),
        metadata: input.metadata,
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
    };
    state
        .backend()
        .retrieval_store(&store_id)
        .await
        .unwrap()
        .upsert_chunks(vec![chunk])
        .await
        .unwrap();
}

/// Seed `count` distinct chunks into a single fresh store/source, all sharing
/// enough vocabulary that a single query matches every one of them (needed to
/// exercise pagination, which requires a candidate pool larger than one page).
///
/// Returns the seeded chunk ids in insertion order (not search rank order).
pub(crate) async fn seed_many_chunks(state: &AppState, count: usize) -> Vec<String> {
    use localdb_core::Embedder;

    state.add_store("store-A", "private").await.unwrap();
    let source = state
        .add_source("store-A", "path", json!({"root": "/tmp"}), "prose", None)
        .await
        .unwrap();
    let store_id = source.store_id.clone();
    let embedder = localdb_core::FakeEmbedder::new(128);

    let mut ids = Vec::with_capacity(count);
    let mut chunks = Vec::with_capacity(count);
    for i in 0..count {
        let chunk_id = format!("chunk-{i:04}");
        let text = format!("pagination test document number {i} rust programming content");
        let docs = vec![localdb_core::embedder::DocumentChunks {
            document_context: text.clone(),
            chunks: vec![text.clone()],
        }];
        let embedding = embedder
            .embed_documents(docs)
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        chunks.push(localdb_core::ChunkRecord {
            id: chunk_id.clone(),
            resource_id: format!("doc-{i:04}"),
            store_id: store_id.clone(),
            text: text.clone(),
            span: localdb_core::types::Span::new(0, text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: format!("hash-{i:04}"),
            origin_store: store_id.clone(),
            source_id: source.id.clone(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: format!("file:///doc{i:04}.md"),
            metadata: localdb_core::metadata::Metadata::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
        });
        ids.push(chunk_id);
    }

    state
        .backend()
        .retrieval_store(&store_id)
        .await
        .unwrap()
        .upsert_chunks(chunks)
        .await
        .unwrap();

    ids
}
