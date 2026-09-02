//! Shared test fixtures for tenant tests.

use localdb_core::metadata::Metadata;
use localdb_core::types::{SourceKind, Span, StoreVisibility};
use localdb_core::{
    ChunkRecord, SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding,
};

use crate::SqliteBackend;

/// Open a fresh backend at `path` and seed it with a single store
/// (`store-1`) and a single path source (`src-1`) — the minimal fixture
/// tenant tests build on before exercising a `TenantStore` handle.
pub(in crate::tenant) async fn backend_with_store_and_source(
    path: &std::path::Path,
) -> SqliteBackend {
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path.to_path_buf(),
        4,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    add_store_and_source(&backend, "store-1", "src-1", "/docs").await;
    backend
}

/// Seed one more store and one path source belonging to it.
///
/// A tenant boundary needs two stores to be a boundary at all, so the
/// fixture above is written in terms of this rather than inlining the two
/// upserts — the second store a cross-tenant test needs is then one call,
/// not a copy of the first store's twenty lines.
pub(in crate::tenant) async fn add_store_and_source(
    backend: &SqliteBackend,
    store_id: &str,
    source_id: &str,
    root: &str,
) {
    backend
        .upsert_store(&StoreRow {
            id: store_id.to_string(),
            name: format!("notes-{store_id}"),
            visibility: StoreVisibility::Private,
            backend: "libsql".to_string(),
            indexing_policy: "{}".to_string(),
            policy_version: "v1".to_string(),
            acl: "{}".to_string(),
            created_at: "2026-07-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();
    backend
        .upsert_source(&SourceRow {
            id: source_id.to_string(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            root: Some(root.to_string()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: "2026-07-01T00:00:00Z".to_string(),
            config_json: None,
            feed_etag: None,
            feed_last_modified: None,
            feed_inputs_digest: None,
        })
        .await
        .unwrap();
}

pub(in crate::tenant) fn chunk_record(fetched_at: &str, modified_at: Option<&str>) -> ChunkRecord {
    ChunkRecord {
        id: "chunk-1".to_string(),
        resource_id: "doc-1".to_string(),
        store_id: "store-1".to_string(),
        text: "some chunk text".to_string(),
        span: Span::new(0, 15),
        heading_path: vec![],
        embedding: vec![0.1, 0.2, 0.3, 0.4],
        policy_version: "v1".to_string(),
        fetched_at: fetched_at.to_string(),
        modified_at: modified_at.map(str::to_string),
        content_hash: "abc123".to_string(),
        origin_store: "store-1".to_string(),
        source_id: "src-1".to_string(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: "file:///docs/doc.md".to_string(),
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

/// Read `date_original`, `date_parsed`, `external_id`, and `external_etag`
/// straight off the `resources` row for `resource_id` — write-only
/// `ChunkRecord` stamps (`core::store::ChunkRecord`'s doc comment) that
/// `get_chunk`/`CHUNK_COLS` never read back, so these tests go around the
/// `RetrievalStore` trait for assertions, same posture as `resource_row`.
pub(in crate::tenant) async fn resource_dates_and_external(
    backend: &SqliteBackend,
    resource_id: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let conn = backend.conn.reader();
    let mut rows = conn
        .query(
            "SELECT date_original, date_parsed, external_id, external_etag FROM resources \
             WHERE store_id = 'store-1' AND id = ?",
            libsql::params![resource_id.to_string()],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("resource row must exist");
    (
        row.get(0).unwrap(),
        row.get(1).unwrap(),
        row.get(2).unwrap(),
        row.get(3).unwrap(),
    )
}
