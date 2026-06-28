//! Axum route handlers for the HTTP API.
//!
//! Every handler receives `State<AppState>` and returns a JSON response or
//! `ApiError`. The URL paths follow the resource list in specs/05-surfaces.md §3.
//!
//! Routes mounted at `/v1`:
//!   GET  /stores                  — list stores
//!   POST /stores                  — create runtime-owned store
//!   GET  /stores/:name            — get store by name
//!   PATCH /stores/:name           — update runtime-owned store
//!   DELETE /stores/:name          — delete runtime-owned store
//!   GET  /stores/:name/sources    — list sources for a store
//!   POST /stores/:name/sources    — add source to a store
//!   DELETE /sources/:id           — remove a source by ID
//!   GET  /documents/:id           — get document by ID
//!   POST /search                  — hybrid search
//!   POST /jobs                    — submit index job
//!   GET  /jobs/:id                — get job by ID
//!   GET  /status                  — daemon status
//!   GET  /config                  — resolved config

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use localdb_core::parser::DocumentMetadata;
use localdb_core::StoreVisibility;
use localdb_core::{
    Citation, Error as CoreError, IndexJob, IndexJobScope, QueryRequest, SearchOrchestrator,
    StoreHandle as CoreStoreHandle,
};

use crate::error::ApiError;
use crate::state::{AppState, SourceRecord, StoreRecord};

// ---------------------------------------------------------------------------
// Pagination helpers
// ---------------------------------------------------------------------------

/// Cursor-based pagination parameters (from specs/05-surfaces.md §3).
#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

fn parse_cursor(cursor: Option<&str>) -> Result<usize, ApiError> {
    match cursor {
        None => Ok(0),
        Some(s) => s.parse::<usize>().map_err(|_| {
            ApiError(CoreError::InvalidRequest {
                message: format!(
                    "invalid pagination cursor '{s}'; expected a non-negative integer"
                ),
            })
        }),
    }
}

/// A paginated list response.
#[derive(Debug, Serialize)]
pub struct PaginatedList<T: Serialize> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub total: usize,
}

impl<T: Serialize> PaginatedList<T> {
    fn new(mut items: Vec<T>, offset: usize, limit: usize, total: usize) -> Self {
        let next_cursor = if offset + limit < total {
            Some(format!("{}", offset + limit))
        } else {
            None
        };
        items.truncate(limit);
        Self {
            items,
            next_cursor,
            total,
        }
    }
}

// ---------------------------------------------------------------------------
// GET /v1/stores
// ---------------------------------------------------------------------------

pub async fn list_stores(
    State(state): State<AppState>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedList<StoreRecord>>, ApiError> {
    let effective = state.effective_config().await?;
    let offset = parse_cursor(pagination.cursor.as_deref())?;

    let all: Vec<StoreRecord> = effective
        .stores
        .iter()
        .map(|s| StoreRecord {
            name: s.name.clone(),
            visibility: s.visibility.clone(),
            backend: s.backend.clone(),
        })
        .collect();

    let total = all.len();
    let page = all.into_iter().skip(offset).collect::<Vec<_>>();
    Ok(Json(PaginatedList::new(
        page,
        offset,
        pagination.limit,
        total,
    )))
}

// ---------------------------------------------------------------------------
// POST /v1/stores
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateStoreRequest {
    pub name: String,
    #[serde(default = "default_private")]
    pub visibility: String,
}

fn default_private() -> String {
    "private".to_string()
}

pub async fn create_store(
    State(state): State<AppState>,
    Json(req): Json<CreateStoreRequest>,
) -> Result<(StatusCode, Json<StoreRecord>), ApiError> {
    if req.name.is_empty() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: "store name cannot be empty".to_string(),
        }));
    }

    let store = state.add_store(&req.name, &req.visibility).await?;
    let visibility = match store.visibility {
        StoreVisibility::Private => "private".to_string(),
        StoreVisibility::Shared => "shared".to_string(),
    };
    let record = StoreRecord {
        name: store.name.clone(),
        visibility,
        backend: store.backend.kind.clone(),
    };
    Ok((StatusCode::CREATED, Json(record)))
}

// ---------------------------------------------------------------------------
// GET /v1/stores/{name}
// ---------------------------------------------------------------------------

pub async fn get_store(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<StoreRecord>, ApiError> {
    let record = state.get_store_by_name(&name).await?;
    Ok(Json(record))
}

// ---------------------------------------------------------------------------
// PATCH /v1/stores/{name}
// ---------------------------------------------------------------------------

/// Request body for PATCH /stores/{name}.
///
/// All fields are optional — only provided fields are updated.
#[derive(Debug, Deserialize)]
pub struct PatchStoreRequest {
    /// New visibility value ("private" | "shared").
    pub visibility: Option<String>,
}

pub async fn patch_store(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<PatchStoreRequest>,
) -> Result<Json<StoreRecord>, ApiError> {
    state.update_store(&name, req.visibility.as_deref()).await?;
    let record = state.get_store_by_name(&name).await?;
    Ok(Json(record))
}

// ---------------------------------------------------------------------------
// DELETE /v1/stores/{name}
// ---------------------------------------------------------------------------

pub async fn delete_store(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.remove_store(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// GET /v1/documents/{id}
// ---------------------------------------------------------------------------

/// Document record returned by the API.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentRecord {
    pub id: String,
    pub uri: String,
    pub title: Option<String>,
    pub store_id: String,
    pub source_id: String,
    pub content_hash: String,
    pub fetched_at: String,
    pub normalized_text: String,
    pub metadata: DocumentMetadata,
}

pub async fn get_document(
    State(state): State<AppState>,
    Path(doc_id): Path<String>,
) -> Result<Json<DocumentRecord>, ApiError> {
    let info = state
        .backend()
        .find_document(&doc_id)
        .await
        .map_err(ApiError)?
        .ok_or(ApiError(CoreError::DocumentNotFound { id: doc_id.clone() }))?;
    let handle = state
        .backend()
        .retrieval_store(&info.store_id)
        .await
        .map_err(ApiError)?;
    let chunks = handle
        .get_chunks_for_document(&info.id)
        .await
        .map_err(ApiError)?;
    let normalized_text = chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Json(DocumentRecord {
        id: info.id,
        uri: info.uri,
        title: info.title,
        store_id: info.store_id,
        source_id: info.source_id,
        content_hash: info.content_hash,
        fetched_at: info.fetched_at,
        normalized_text,
        metadata: info.metadata,
    }))
}

// ---------------------------------------------------------------------------
// GET /v1/stores/{name}/sources
// ---------------------------------------------------------------------------

pub async fn list_sources(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Query(pagination): Query<PaginationParams>,
) -> Result<Json<PaginatedList<SourceRecord>>, ApiError> {
    let offset = parse_cursor(pagination.cursor.as_deref())?;

    let all = state.list_sources(&store_name).await?;
    let total = all.len();
    let page = all.into_iter().skip(offset).collect::<Vec<_>>();
    Ok(Json(PaginatedList::new(
        page,
        offset,
        pagination.limit,
        total,
    )))
}

// ---------------------------------------------------------------------------
// POST /v1/stores/{name}/sources
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSourceRequest {
    pub kind: String,
    pub spec: serde_json::Value,
    #[serde(default = "default_prose")]
    pub preset: String,
    pub refresh: Option<String>,
}

fn default_prose() -> String {
    "prose".to_string()
}

pub async fn create_source(
    State(state): State<AppState>,
    Path(store_name): Path<String>,
    Json(req): Json<CreateSourceRequest>,
) -> Result<(StatusCode, Json<SourceRecord>), ApiError> {
    if req.kind != "path" && req.kind != "url" {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!(
                "unknown source kind '{}'; expected 'path' or 'url'",
                req.kind
            ),
        }));
    }

    let source = state
        .add_source(
            &store_name,
            &req.kind,
            req.spec,
            &req.preset,
            req.refresh.as_deref(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(source)))
}

// ---------------------------------------------------------------------------
// DELETE /v1/sources/{id}
// ---------------------------------------------------------------------------

pub async fn delete_source(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.remove_source(&source_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// POST /v1/search
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub store_filter: Vec<String>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_search_limit() -> usize {
    10
}

/// Search response.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub citations: Vec<Citation>,
    pub total_candidates: usize,
    pub next_cursor: Option<String>,
}

pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    if req.query.is_empty() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: "query cannot be empty".to_string(),
        }));
    }

    let offset = parse_cursor(req.cursor.as_deref())?;

    let effective = state.effective_config().await?;
    for name in &req.store_filter {
        if !effective.stores.iter().any(|s| s.name == *name) {
            return Err(ApiError(CoreError::StoreNotFound { id: name.clone() }));
        }
    }

    let yaml = state.yaml_config().await;
    let embed_policy = &yaml.defaults.indexing.embedding;

    let embedder: Box<dyn localdb_core::Embedder> = embed::create_embedder(
        embed_policy,
        &yaml.providers,
        None, // models_dir not available in server context; use cache default
    )
    .map_err(|e| {
        ApiError(CoreError::InvalidConfig {
            message: e.to_string(),
        })
    })?;

    let target_stores: Vec<_> = if req.store_filter.is_empty() {
        effective.stores.iter().collect()
    } else {
        effective
            .stores
            .iter()
            .filter(|s| req.store_filter.contains(&s.name))
            .collect()
    };

    let mut store_handles: Vec<CoreStoreHandle> = Vec::new();

    for store_cfg in target_stores {
        let store_id = store_cfg.id.clone();
        let handle = state
            .backend()
            .retrieval_store(&store_id)
            .await
            .map_err(ApiError)?;
        store_handles.push(CoreStoreHandle {
            id: store_id,
            name: store_cfg.name.clone(),
            store: handle,
        });
    }

    if store_handles.is_empty() {
        return Ok(Json(SearchResponse {
            citations: vec![],
            total_candidates: 0,
            next_cursor: None,
        }));
    }

    let query_request = QueryRequest {
        query: req.query.clone(),
        leg_k: None,
        top_n: Some(req.limit),
        filters: vec![],
    };

    let response = SearchOrchestrator::query(&store_handles, embedder.as_ref(), &query_request)
        .await
        .map_err(ApiError)?;

    let total = response.total_candidates;
    let next_cursor = if offset + req.limit < total {
        Some(format!("{}", offset + req.limit))
    } else {
        None
    };

    Ok(Json(SearchResponse {
        citations: response.citations,
        total_candidates: total,
        next_cursor,
    }))
}

// ---------------------------------------------------------------------------
// POST /v1/jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub store_name: String,
    #[serde(default)]
    pub source_id: Option<String>,
}

pub async fn create_job(
    State(state): State<AppState>,
    Json(req): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<IndexJob>), ApiError> {
    // Verify store exists
    let effective = state.effective_config().await?;
    let _store = effective
        .stores
        .iter()
        .find(|s| s.name == req.store_name)
        .ok_or_else(|| CoreError::StoreNotFound {
            id: req.store_name.clone(),
        })?;

    let scope = if let Some(source_id) = &req.source_id {
        IndexJobScope::Source {
            source_id: source_id.clone(),
        }
    } else {
        IndexJobScope::Store
    };

    // Submit a no-op job (real ingestion is wired by the daemon startup).
    // In integration tests, the task closure can be swapped with a real pipeline run.
    let job = state
        .job_queue()
        .submit(&req.store_name, scope, || {
            Ok(localdb_core::IndexJobStats::default())
        })
        .await;

    Ok((StatusCode::ACCEPTED, Json(job)))
}

// ---------------------------------------------------------------------------
// GET /v1/jobs/{id}
// ---------------------------------------------------------------------------

pub async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<IndexJob>, ApiError> {
    state
        .job_queue()
        .get_job(&job_id)
        .await
        .map(Json)
        .ok_or(ApiError(CoreError::JobNotFound { id: job_id }))
}

// ---------------------------------------------------------------------------
// GET /v1/status
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub daemon: bool,
    pub store_count: usize,
    pub source_count: usize,
    pub job_count: usize,
}

pub async fn get_status(State(state): State<AppState>) -> Result<Json<StatusResponse>, ApiError> {
    let effective = state.effective_config().await?;
    let store_count = effective.stores.len();

    let mut source_count = 0;
    for store in &effective.stores {
        let sources = state.list_sources(&store.name).await?;
        source_count += sources.len();
    }

    let jobs = state.job_queue().list_jobs().await;

    Ok(Json(StatusResponse {
        daemon: true,
        store_count,
        source_count,
        job_count: jobs.len(),
    }))
}

// ---------------------------------------------------------------------------
// GET /v1/config
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    pub yaml_config: serde_json::Value,
    pub effective_stores: Vec<EffectiveStoreView>,
}

#[derive(Debug, Serialize)]
pub struct EffectiveStoreView {
    pub name: String,
    pub visibility: String,
    pub backend: String,
}

pub async fn get_config(State(state): State<AppState>) -> Result<Json<ConfigResponse>, ApiError> {
    let yaml = state.yaml_config().await;
    let effective = state.effective_config().await?;

    let yaml_value = serde_json::to_value(&yaml).map_err(|e| {
        ApiError(CoreError::Internal {
            message: format!("cannot serialize config: {}", e),
            correlation_id: "config_serialize".to_string(),
        })
    })?;

    let effective_stores = effective
        .stores
        .iter()
        .map(|s| EffectiveStoreView {
            name: s.name.clone(),
            visibility: s.visibility.clone(),
            backend: s.backend.clone(),
        })
        .collect();

    Ok(Json(ConfigResponse {
        yaml_config: yaml_value,
        effective_stores,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
        routing::{delete, get, post},
        Router,
    };
    use serde_json::json;
    use tempfile::TempDir;
    use tower::ServiceExt; // for `oneshot`

    async fn make_app() -> (TempDir, Router) {
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
            .with_state(state);

        (dir, router)
    }

    async fn json_body(body: axum::body::Body) -> serde_json::Value {
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn make_state_with_fake_config() -> (TempDir, AppState) {
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
        )
        .await
        .unwrap();
        (dir, state)
    }

    struct SeedChunkInput {
        chunk_id: &'static str,
        doc_id: &'static str,
        text: &'static str,
        uri: &'static str,
        metadata: localdb_core::DocumentMetadata,
    }

    async fn seed_store_a_chunk(state: &AppState, input: SeedChunkInput) {
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
            document_id: input.doc_id.to_string(),
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
            source_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: input.uri.to_string(),
            metadata: input.metadata,
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

    // --- List stores ---

    #[tokio::test]
    async fn list_stores_empty() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/stores")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["items"].as_array().unwrap().len(), 0);
    }

    // --- Create store ---

    #[tokio::test]
    async fn create_store_returns_201() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "my-notes"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["name"], "my-notes");
    }

    #[tokio::test]
    async fn create_store_empty_name_returns_400() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": ""}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_store_invalid_visibility_returns_400() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"name": "my-notes", "visibility": "public"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // --- Get store ---

    #[tokio::test]
    async fn get_store_not_found_returns_404() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/stores/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- Delete store ---

    #[tokio::test]
    async fn delete_store_not_found_returns_409_or_404() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/v1/stores/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // StoreNotFound → 404
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- Sources CRUD ---

    #[tokio::test]
    async fn source_crud_roundtrip() {
        let (_dir, app) = make_app().await;

        // Create store
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "docs"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Add source
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores/docs/sources")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "kind": "path",
                            "spec": {"root": "/tmp/docs", "include": [], "exclude": []},
                            "preset": "prose"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp.into_body()).await;
        let source_id = body["id"].as_str().unwrap().to_string();

        // List sources
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/stores/docs/sources")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["items"].as_array().unwrap().len(), 1);

        // The source ID is known (we just verified its existence in the list)
        let _ = source_id; // full delete tested in separate integration test
    }

    // --- Search ---

    #[tokio::test]
    async fn search_empty_query_returns_400() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"query": ""}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_with_nonexistent_store_filter_returns_404() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"query": "hello", "store_filter": ["no-such-store"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn search_returns_citations_shape() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"query": "hello world"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert!(body["citations"].is_array());
        assert!(body["total_candidates"].is_number());
    }

    // --- Jobs ---

    #[tokio::test]
    async fn post_job_returns_202() {
        let (_dir, app) = make_app().await;

        // Create store first
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "test"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"store_name": "test"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = json_body(resp.into_body()).await;
        assert!(body["id"].as_str().is_some());
    }

    #[tokio::test]
    async fn get_job_not_found_returns_404() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/jobs/nonexistent-job-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_and_poll_job() {
        let (_dir, app) = make_app().await;

        // Create store
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "test"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Submit job
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"store_name": "test"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(resp.into_body()).await;
        let job_id = body["id"].as_str().unwrap().to_string();

        // Poll until done
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("job did not complete in time");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;

            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/v1/jobs/{}", job_id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = json_body(resp.into_body()).await;
            if body["state"] == "done" {
                break;
            }
        }
    }

    // --- Status ---

    #[tokio::test]
    async fn get_status_returns_daemon_true() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["daemon"], true);
    }

    // --- Config ---

    #[tokio::test]
    async fn get_config_returns_yaml_config() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert!(body["yaml_config"].is_object());
        assert!(body["effective_stores"].is_array());
    }

    // --- PATCH /stores/{name} ---

    #[tokio::test]
    async fn patch_store_updates_visibility() {
        let (_dir, app) = make_app().await;

        // Create a runtime-owned store
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "my-store"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Patch visibility to "shared"
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/v1/stores/my-store")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"visibility": "shared"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["visibility"], "shared");
    }

    #[tokio::test]
    async fn patch_store_not_found_returns_404() {
        let (_dir, app) = make_app().await;

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/v1/stores/no-such-store")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"visibility": "shared"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- GET /documents/{id} ---

    #[tokio::test]
    async fn get_document_not_found_returns_404() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/documents/nonexistent-doc-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["code"], "document_not_found");
    }

    #[tokio::test]
    async fn get_document_returns_record_when_indexed() {
        let (_dir, state) = make_state_with_fake_config().await;
        let metadata = localdb_core::parser::DocumentMetadata {
            title: Some("Test Doc".to_string()),
            creator: vec!["Test Author".to_string()],
            date: Some("2026-06-10".to_string()),
            ..Default::default()
        };
        seed_store_a_chunk(
            &state,
            SeedChunkInput {
                chunk_id: "chunk-doc-abc123",
                doc_id: "doc-abc123",
                text: "hello world",
                uri: "file:///test.md",
                metadata,
            },
        )
        .await;

        let app = crate::daemon::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/documents/doc-abc123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["id"], "doc-abc123");
        assert_eq!(body["uri"], "file:///test.md");
        assert_eq!(body["title"], "Test Doc");
        assert_eq!(body["normalized_text"], "hello world");
        assert!(
            body.get("metadata").is_some(),
            "metadata field must be present"
        );
        assert_eq!(
            body["metadata"]["creator"].as_array().unwrap()[0]
                .as_str()
                .unwrap(),
            "Test Author"
        );
    }

    // --- /search returns citations (AC) ---

    #[tokio::test]
    async fn search_returns_citations_after_indexing() {
        let (_dir, state) = make_state_with_fake_config().await;
        seed_store_a_chunk(
            &state,
            SeedChunkInput {
                chunk_id: "chunk-1",
                doc_id: "doc-1",
                text: "hello world rust programming",
                uri: "file:///hello.md",
                metadata: localdb_core::DocumentMetadata::default(),
            },
        )
        .await;

        let app = crate::daemon::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"query": "hello world"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        let citations = body["citations"].as_array().unwrap();
        assert!(
            !citations.is_empty(),
            "/search should return citations after indexing a chunk, got: {:?}",
            body
        );
        assert_eq!(citations[0]["uri"], "file:///hello.md");
    }

    // --- search with non-existent store_filter returns empty, not foreign results ---

    #[tokio::test]
    async fn search_with_nonexistent_store_filter_returns_empty() {
        let (_dir, state) = make_state_with_fake_config().await;
        state.add_store("my-store", "private").await.unwrap();
        seed_store_a_chunk(
            &state,
            SeedChunkInput {
                chunk_id: "chunk-ff",
                doc_id: "doc-ff",
                text: "hello world",
                uri: "file:///foreign.md",
                metadata: localdb_core::DocumentMetadata::default(),
            },
        )
        .await;

        let app = crate::daemon::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"query": "hello world", "store_filter": ["my-store"]}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        let citations = body["citations"].as_array().unwrap();
        assert!(
            citations.is_empty(),
            "store_filter for a store with no on-disk data should return empty, not foreign results; got: {:?}",
            body
        );
    }

    // --- DELETE /sources/{id} ---

    #[tokio::test]
    async fn delete_source_removes_it() {
        let (_dir, app) = make_app().await;

        // Create store and source
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores")
                    .header("content-type", "application/json")
                    .body(Body::from(json!({"name": "mystore"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/stores/mystore/sources")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "kind": "path",
                            "spec": {"root": "/tmp/mystore", "include": [], "exclude": []},
                            "preset": "prose"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body = json_body(resp.into_body()).await;
        let source_id = body["id"].as_str().unwrap().to_string();

        // Delete the source
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/v1/sources/{}", source_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify it's gone
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/stores/mystore/sources")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["items"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn delete_nonexistent_source_returns_404() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/v1/sources/nonexistent-src-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["code"], "source_not_found");
    }

    // --- POST /jobs with nonexistent store ---

    #[tokio::test]
    async fn create_job_nonexistent_store_returns_404() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"store_name": "no-such-store"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["code"], "store_not_found");
    }

    // --- Pagination cursors ---

    #[tokio::test]
    async fn pagination_cursor_works() {
        let (_dir, app) = make_app().await;

        // Create 3 stores
        for name in &["alpha", "beta", "gamma"] {
            app.clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/v1/stores")
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"name": *name}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
        }

        // Get first page (limit=2)
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/stores?limit=2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["items"].as_array().unwrap().len(), 2);
        assert!(body["next_cursor"].is_string());
        let cursor = body["next_cursor"].as_str().unwrap().to_string();

        // Get second page using cursor
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/stores?limit=2&cursor={}", cursor))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp.into_body()).await;
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert!(body["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn list_stores_invalid_cursor_returns_400() {
        let (_dir, app) = make_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/stores?cursor=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
