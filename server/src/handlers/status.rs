use axum::{extract::State, Json};
use serde::Serialize;

use localdb_core::TableSize;

use crate::error::ApiError;
use crate::state::AppState;

/// One store's status figures, mirroring the embedded CLI's
/// `cmds::status::StoreStatusEntry` (issue #187 stage 5) — the two must stay
/// in lockstep or `localdb status` would render different numbers depending
/// on whether a daemon happens to be running.
#[derive(Debug, Serialize)]
pub struct StoreStatusRecord {
    pub name: String,
    pub visibility: String,
    pub backend: String,
    /// `None` when `RetrievalStore::stats()` failed for this store (e.g. a
    /// corrupt or mid-migration store) — `status` must keep reporting on the
    /// daemon state and the other stores rather than aborting outright.
    pub document_count: Option<u64>,
    pub chunk_count: Option<u64>,
}

/// The shared `localdb.db` file's on-disk figures — reported once, not
/// per-store (specs/03-config.md: one physical file backs every store).
#[derive(Debug, Serialize)]
pub struct DatabaseStatus {
    pub path: String,
    pub exists: bool,
    pub size_bytes: Option<u64>,
    pub wal_size_bytes: Option<u64>,
    pub total_size_bytes: u64,
    pub bytes_per_chunk: Option<u64>,
    pub largest_tables: Vec<TableSize>,
}

/// How many rows `largest_tables` reports — matches the embedded CLI's
/// `cmds::status::LARGEST_TABLES_LIMIT`.
const LARGEST_TABLES_LIMIT: usize = 5;

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub daemon: bool,
    pub store_count: usize,
    pub source_count: usize,
    pub job_count: usize,
    /// Per-store figures (issue #187 stage 5) — added so the CLI's
    /// daemon-routed `status` can render identically to embedded `status`
    /// instead of only reporting a bare `store_count`.
    pub stores: Vec<StoreStatusRecord>,
    pub database: DatabaseStatus,
}

pub async fn get_status(State(state): State<AppState>) -> Result<Json<StatusResponse>, ApiError> {
    let effective = state.effective_config().await?;
    let store_count = effective.stores.len();

    let mut source_count = 0;
    let mut stores = Vec::with_capacity(effective.stores.len());
    for store in &effective.stores {
        let sources = state.list_sources(&store.name).await?;
        source_count += sources.len();

        // Best-effort, exactly like the embedded path's
        // `gather_store_status`: a single corrupt or mid-migration store
        // must not blank out the whole status report.
        let stats = match state.backend().retrieval_store(&store.id).await {
            Ok(retrieval) => retrieval.stats().await.ok(),
            Err(_) => None,
        };
        stores.push(StoreStatusRecord {
            name: store.name.clone(),
            visibility: store.visibility.clone(),
            backend: store.backend.clone(),
            document_count: stats.as_ref().map(|s| s.document_count),
            chunk_count: stats.as_ref().map(|s| s.chunk_count),
        });
    }

    let jobs = state.job_queue().list_jobs().await;

    let db_path = state.data_dir().join("localdb.db");
    let db_size = localdb_core::compute_db_file_size(&db_path);
    let total_chunks: u64 = stores.iter().filter_map(|s| s.chunk_count).sum();
    let largest_tables = state
        .backend()
        .largest_tables(LARGEST_TABLES_LIMIT)
        .await
        .unwrap_or_default();

    Ok(Json(StatusResponse {
        daemon: true,
        store_count,
        source_count,
        job_count: jobs.len(),
        stores,
        database: DatabaseStatus {
            path: db_path.display().to_string(),
            exists: db_size.main_bytes.is_some(),
            size_bytes: db_size.main_bytes,
            wal_size_bytes: db_size.wal_bytes,
            total_size_bytes: db_size.total_bytes(),
            bytes_per_chunk: localdb_core::bytes_per_chunk(db_size.total_bytes(), total_chunks),
            largest_tables,
        },
    }))
}
