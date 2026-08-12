use axum::{extract::State, Json};
use axum_extra::extract::Query;
use serde::{Deserialize, Serialize};

use localdb_core::{Error, TableSize};

use crate::error::ApiError;
use crate::state::{AppState, EffectiveStore};

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

/// `GET /status` query params: a repeatable `?store=` scopes the response to
/// specific stores, mirroring CLI `--store` (issue #187 review, finding F7).
/// `Vec<String>` + `#[serde(default)]`, not `Option<Vec<_>>` —
/// `axum_extra::extract::Query`'s own guidance for correctly handling zero,
/// one, or many repeated params of the same name.
#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    #[serde(default)]
    pub store: Vec<String>,
}

/// Resolve `?store=` names against the DB-backed store list. Non-empty
/// `names` scopes to exactly those stores (request order, duplicates
/// collapsed); an unknown name is `Error::StoreNotFound` (→ 404), matching
/// the embedded CLI's exit code 3 for an unresolvable explicit `--store`
/// name. Empty `names` is the pre-existing "every store" behavior.
fn resolve_status_scope(
    all_stores: Vec<EffectiveStore>,
    names: &[String],
) -> Result<Vec<EffectiveStore>, Error> {
    if names.is_empty() {
        return Ok(all_stores);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let store = all_stores
            .iter()
            .find(|s| &s.name == name)
            .cloned()
            .ok_or_else(|| Error::StoreNotFound { id: name.clone() })?;
        out.push(store);
    }
    Ok(out)
}

pub async fn get_status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, ApiError> {
    let effective = state.effective_config().await?;
    let scoped_stores = resolve_status_scope(effective.stores, &query.store)?;
    let store_count = scoped_stores.len();

    let mut source_count = 0;
    let mut stores = Vec::with_capacity(scoped_stores.len());
    for store in &scoped_stores {
        // Best-effort, exactly like the stats call just below: a single
        // store's source listing failing (e.g. a corrupt or mid-migration
        // store) must not blank out the whole status report — it simply
        // doesn't contribute to `source_count`.
        if let Ok(sources) = state.list_sources(&store.name).await {
            source_count += sources.len();
        }

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
