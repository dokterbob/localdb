use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::metadata::Metadata;
use crate::store::RetrievalStore;
use crate::types::{SourceKind, StoreVisibility};
use crate::{Error, VectorEncoding};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreBackendConnection {
    LocalPath(PathBuf),
    Url(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreBackendConfig {
    pub connection: StoreBackendConnection,
    pub embedding_dim: usize,
    pub encoding: VectorEncoding,
}

impl StoreBackendConfig {
    pub fn local_path(path: PathBuf, embedding_dim: usize, encoding: VectorEncoding) -> Self {
        Self {
            connection: StoreBackendConnection::LocalPath(path),
            embedding_dim,
            encoding,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreRow {
    pub id: String,
    pub name: String,
    pub visibility: StoreVisibility,
    pub backend: String,
    pub indexing_policy: String,
    pub policy_version: String,
    pub acl: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRow {
    pub id: String,
    pub store_id: String,
    pub kind: SourceKind,
    pub root: Option<String>,
    pub url: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub preset: String,
    pub refresh: Option<String>,
    pub created_at: String,
    /// Kind-specific JSON config blob. Currently populated only for feed
    /// sources (`{"max_entries": null|N, "fetch_full_content": bool}`, see
    /// `core::source::{parse_feed_config_json, build_feed_config_json}`);
    /// `None` for path/url sources. The `refresh` interval is NOT stored
    /// here even for feed sources — it stays in the `refresh` column as the
    /// single source of truth read directly by the scheduler.
    pub config_json: Option<String>,
    /// The feed document's own conditional-GET `ETag`, when one is stored.
    ///
    /// Populated only for feed sources, and only once a run has actually
    /// fetched the feed document with a 200 or 304 response that carried
    /// one. In discovery mode the feed document itself never becomes a
    /// `Resource` — there is no `resources` row to hang a validator on — so
    /// these columns live here instead (specs/02-domain-model.md's Feed
    /// connector, "Conditional GET and pruning"). Distinct from an entry's
    /// own `resources.external_etag`, which this column has no bearing on.
    /// Must be nulled whenever the source's `url` changes: a validator is
    /// only meaningful against the origin that issued it.
    pub feed_etag: Option<String>,
    /// The feed document's own conditional-GET `Last-Modified`, alongside
    /// [`Self::feed_etag`]. Same population rule, same null-on-URL-change
    /// requirement.
    pub feed_last_modified: Option<String>,
    /// `crate::ids::compute_feed_inputs_digest` of the local inputs in force
    /// when the two validators above were stored.
    ///
    /// The validators speak for the origin's bytes; this speaks for our own
    /// side of the pipeline. A run whose inputs no longer match refuses to
    /// replay them and refetches unconditionally, because an origin has no
    /// way to signal that *our* indexing policy changed. `None` means
    /// "inputs unknown" — every row predating this column — and mismatches
    /// any current digest, so such a row refetches once rather than trusting
    /// a validator captured before the guard existed.
    ///
    /// Written in the same hop as the validators, never separately: the pair
    /// is one fact ("these validators were captured under these inputs"), and
    /// recording new inputs against validators captured under the old ones
    /// would mark the cache trustworthy for a reprocessing that never
    /// happened.
    pub feed_inputs_digest: Option<String>,
}

/// One row of `StoreBackend::largest_tables`: an on-disk table's name and its
/// aggregate byte size (its own pages plus every index built on it), as
/// reported by SQLite's `dbstat` virtual table.
///
/// A whole-database-file diagnostic, not scoped to any one store — every
/// store shares the same unified `localdb.db` file (specs/03-config.md), so
/// "which table is biggest" is a property of the file, not of a store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableSize {
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentInfo {
    pub store_id: String,
    pub id: String,
    pub source_id: String,
    pub ingestor_kind: String,
    pub uri: String,
    pub title: Option<String>,
    pub mime: Option<String>,
    pub content_hash: String,
    pub fetched_at: String,
    pub origin_store: String,
    pub policy_version: String,
    pub metadata: Metadata,

    /// The resource's own claimed date, exactly as the source expressed it
    /// (`resources.date_original`). `None` when the source carried no date
    /// (or, for daemon-attached reads against a pre-v7 database, on a row
    /// written before this column existed).
    pub date_original: Option<String>,

    /// `date_original` normalized to a sortable ISO 8601 string
    /// (`resources.date_parsed`, via `crate::dates::parse_partial_iso8601`).
    /// `None` when `date_original` was absent or unparseable.
    pub date_parsed: Option<String>,

    /// Write-time clock of this resource's last index (`resources.
    /// index_updated_at`, migration v7). `Option` because a daemon may open
    /// a pre-v7 database before `localdb db migrate` has run — `open` never
    /// migrates on any surface (docs/migrations.md).
    pub index_updated_at: Option<String>,
}

#[async_trait]
pub trait StoreBackend: Send + Sync + 'static {
    async fn open(config: StoreBackendConfig) -> Result<Self, Error>
    where
        Self: Sized;

    async fn upsert_store(&self, store: &StoreRow) -> Result<(), Error>;
    async fn delete_store(&self, id: &str) -> Result<bool, Error>;
    async fn get_store(&self, id: &str) -> Result<Option<StoreRow>, Error>;
    async fn get_store_by_name(&self, name: &str) -> Result<Option<StoreRow>, Error>;
    async fn list_stores(&self) -> Result<Vec<StoreRow>, Error>;

    async fn upsert_source(&self, source: &SourceRow) -> Result<(), Error>;
    async fn delete_source(&self, id: &str) -> Result<bool, Error>;
    async fn get_source(&self, id: &str) -> Result<Option<SourceRow>, Error>;
    async fn list_sources(&self, store_id: &str) -> Result<Vec<SourceRow>, Error>;
    async fn find_source_by_root_or_url(
        &self,
        value: &str,
        store_id: &str,
    ) -> Result<Option<SourceRow>, Error>;

    /// Rewrite just a feed source's transport cache columns —
    /// `feed_etag`, `feed_last_modified`, `feed_inputs_digest` — leaving
    /// every other column of the row alone. Returns whether a row was
    /// actually updated.
    ///
    /// Update-only, deliberately, where [`Self::upsert_source`] would do:
    /// this runs at the end of an index job, from a `SourceRow` snapshot
    /// taken when the job started. A `source delete` landing in between
    /// would make an upsert **re-insert** the row — silently undoing a
    /// deletion the scheduler has already acted on. `Ok(false)` is that
    /// race, reported rather than papered over; it is not an error, and the
    /// job that sees it has nothing left to do. `upsert_source` keeps its
    /// insert-or-update semantics for the callers that legitimately need
    /// them.
    ///
    /// The three columns move together because they are one fact — see
    /// [`SourceRow::feed_inputs_digest`].
    async fn update_source_feed_cache(
        &self,
        id: &str,
        feed_etag: Option<&str>,
        feed_last_modified: Option<&str>,
        feed_inputs_digest: Option<&str>,
    ) -> Result<bool, Error>;

    /// Look up a single document by id.
    ///
    /// `store_id: Some(_)` scopes the lookup to that store — the query itself
    /// carries the filter, so a document id shared across stores never
    /// ambiguates. `None` looks up the id across every store; if more than
    /// one store holds a document with that id, implementations return
    /// `Error::InvalidRequest` rather than guessing which one the caller
    /// meant.
    async fn find_document(
        &self,
        doc_id: &str,
        store_id: Option<&str>,
    ) -> Result<Option<DocumentInfo>, Error>;

    /// List documents in `store_id`, ordered by `uri`, optionally filtered to
    /// a single `source_id`, and paginated by `limit`/`offset`.
    ///
    /// `limit: None` returns every row from `offset` onward, uncapped.
    /// `offset` past the end of the (filtered) result set yields an empty
    /// list, not an error. An unknown `source_id` is a pure filter — it
    /// yields an empty list rather than an error, matching `find_document`'s
    /// "no error on a miss" posture for read paths.
    async fn list_documents(
        &self,
        store_id: &str,
        source_id: Option<&str>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Vec<DocumentInfo>, Error>;

    /// Count documents in `store_id`, optionally filtered to a single
    /// `source_id` — the un-paginated total backing a paginated
    /// `list_documents` call's envelope. Same "unknown `source_id` is a pure
    /// filter" posture as `list_documents`.
    async fn count_documents(&self, store_id: &str, source_id: Option<&str>) -> Result<u64, Error>;

    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error>;

    /// The largest on-disk tables (own pages + every index on them) by byte
    /// size, descending, capped at `limit`.
    ///
    /// A best-effort diagnostic for `localdb status` (issues #179, #177):
    /// implementations that can't compute it (e.g. the backend's `dbstat`
    /// equivalent is unavailable) return an empty vector rather than
    /// erroring — callers must treat an empty result as "unavailable", not
    /// as "the database has no tables".
    async fn largest_tables(&self, limit: usize) -> Result<Vec<TableSize>, Error>;
}

/// Resolve an explicit list of store names against `backend`, in request
/// order, with duplicate names collapsed to their first occurrence.
///
/// Shared by the CLI's `resolve_store_scope_inner`
/// (`cli/src/app_db.rs`) and the daemon's `resolve_status_scope`
/// (`server/src/handlers/status.rs`, issue #187 PR #212) — both need the
/// same "explicit `--store`/`?store=` names, in request order, deduplicated,
/// `Error::StoreNotFound` on a miss" resolution against `dyn StoreBackend`.
/// The CLI layers its own `StoreScopePolicy` (all-stores / default-store
/// fallbacks for when no names were given) on top; that policy logic is not
/// part of this helper, which only resolves a name list into rows.
///
/// `names` is expected to be non-empty (both callers branch on
/// `names.is_empty()` before reaching here, since an empty list means
/// something different at each call site); an empty slice simply returns an
/// empty vector without touching `backend`.
pub async fn resolve_named_stores(
    backend: &dyn StoreBackend,
    names: &[String],
) -> Result<Vec<StoreRow>, Error> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let store = backend
            .get_store_by_name(name)
            .await?
            .ok_or_else(|| Error::StoreNotFound { id: name.clone() })?;
        out.push(store);
    }
    Ok(out)
}
