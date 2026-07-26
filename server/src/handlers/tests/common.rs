use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use serde_json::json;
use tempfile::TempDir;

use crate::auth::middleware::require_auth;
use crate::handlers::{
    create_job, create_source, create_store, delete_source, delete_store, get_config, get_document,
    get_job, get_status, get_store, list_sources, list_stores, patch_store, search,
};
use crate::state::AppState;

pub(crate) async fn make_app() -> (TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let yaml_config = localdb_core::config::schema::RawConfig {
        version: 1,
        server: Default::default(),
        paths: Default::default(),
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
        providers: vec![],
    };
    let queue = crate::job_queue::JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
        crate::auth::AuthMode::Open,
    )
    .await
    .unwrap();

    let router = Router::new()
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
        .route("/v1/jobs/{id}", get(get_job))
        .route("/v1/status", get(get_status))
        .route("/v1/config", get(get_config))
        .with_state(state.clone())
        // Open-mode auth: inserts `Principal::local_trust()` on every
        // request, matching the real `daemon::build_router` — needed since
        // handlers now read the request's `Principal` (D7 scoping,
        // admin-only checks) via `handlers::require_principal`.
        .layer(middleware::from_fn_with_state(state, require_auth));

    (dir, router)
}

pub(crate) async fn json_body(body: axum::body::Body) -> serde_json::Value {
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

pub(crate) async fn make_state_with_fake_config() -> (TempDir, AppState) {
    make_state_with_auth_mode(crate::auth::AuthMode::Open).await
}

/// Like `make_state_with_fake_config`, but with an explicit auth mode —
/// needed to exercise a real, non-`local_trust` `Principal` end to end
/// (`AuthMode::Enforced`, `require_auth` resolving a bearer token via
/// `state.auth()`) rather than the `Open` mode's always-admin
/// `local_trust` short-circuit.
pub(crate) async fn make_state_with_auth_mode(
    auth_mode: crate::auth::AuthMode,
) -> (TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let yaml_config = localdb_core::config::schema::RawConfig {
        version: 1,
        server: Default::default(),
        paths: Default::default(),
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
        providers: vec![],
    };
    let queue = crate::job_queue::JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        queue.clone(),
        crate::scheduler::UrlRefreshScheduler::new(queue),
        auth_mode,
    )
    .await
    .unwrap();
    (dir, state)
}

/// Create a user of `role` and mint an API key through the state's own
/// `AuthService` — the same persistent database the router serves.
/// Returns the plaintext bearer secret.
pub(crate) async fn seed_user_with_key(
    state: &AppState,
    name: &str,
    role: localdb_core::auth::Role,
) -> String {
    let user = state.auth().create_user(name, role).await.unwrap();
    state.auth().issue_api_key(&user.id).await.unwrap().secret
}

pub(crate) struct SeedChunkInput {
    pub(crate) chunk_id: &'static str,
    pub(crate) doc_id: &'static str,
    pub(crate) text: &'static str,
    pub(crate) uri: &'static str,
    pub(crate) metadata: localdb_core::metadata::Metadata,
}

pub(crate) async fn seed_store_a_chunk(state: &AppState, input: SeedChunkInput) {
    seed_chunk_in_store(state, "store-A", "private", input).await;
}

/// Like `seed_store_a_chunk`, but the owning store's name/visibility are
/// caller-controlled — needed to seed a `shared` store for D7 grant tests
/// (`grant_store` rejects `private` visibility).
pub(crate) async fn seed_chunk_in_store(
    state: &AppState,
    store_name: &str,
    visibility: &str,
    input: SeedChunkInput,
) {
    use localdb_core::Embedder;

    state.add_store(store_name, visibility).await.unwrap();
    let source = state
        .add_source(store_name, "path", json!({"root": "/tmp"}), "prose", None)
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
