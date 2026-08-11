//! Whole-database-file diagnostics for `localdb status` (issues #179, #177).
//!
//! Unlike `registry::stores`/`sources`, these queries are not scoped to a
//! single store — every store shares one physical `localdb.db` file
//! (specs/03-config.md), so "how big is each table" is a property of the
//! file, not of any one store's rows within it.

use localdb_core::{Error, TableSize};

use crate::connection::{map_libsql_err, LibsqlDb};

/// The largest on-disk tables by aggregate byte size, descending.
///
/// Uses SQLite's `dbstat` virtual table (page-level accounting), left-joined
/// against `sqlite_master` so an index's pages are attributed to the table
/// it indexes rather than to the index's own name — that's the number that
/// actually explains "why is this file big" (e.g. `chunks`'s vector index
/// shows up rolled into `chunks`, not as a separate unlabeled row).
///
/// Best-effort: `dbstat` requires SQLite to have been compiled with
/// `SQLITE_ENABLE_DBSTAT_VTAB`. If the query itself fails for any reason,
/// this returns `Ok(vec![])` (after a `tracing::warn!`) rather than
/// propagating the error — this is a diagnostic nice-to-have that `status`
/// must not depend on for its exit code (see `StoreBackend::largest_tables`'s
/// doc comment).
pub(crate) async fn largest_tables(db: &LibsqlDb, limit: usize) -> Result<Vec<TableSize>, Error> {
    let conn = db.conn().await;
    let mut rows = match conn
        .query(
            "SELECT COALESCE(m.tbl_name, d.name) AS table_name, SUM(d.pgsize) AS bytes
             FROM dbstat d
             LEFT JOIN sqlite_master m ON m.name = d.name AND m.type = 'index'
             WHERE d.name NOT LIKE 'sqlite_%'
             GROUP BY table_name
             ORDER BY bytes DESC
             LIMIT ?",
            libsql::params![limit as i64],
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "dbstat query failed; omitting largest-tables diagnostic from status"
            );
            return Ok(Vec::new());
        }
    };

    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        let name: String = row.get(0).map_err(map_libsql_err)?;
        let bytes: i64 = row.get(1).map_err(map_libsql_err)?;
        out.push(TableSize {
            name,
            // pgsize is a page-count-derived byte size and always >= 0 in
            // practice; the max(0) guards against a pathological negative
            // SUM (e.g. an empty group) rather than panicking the `as u64`.
            bytes: bytes.max(0) as u64,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use localdb_core::store::ChunkRecord;
    use localdb_core::types::{SourceKind, Span, StoreVisibility};
    use localdb_core::{SourceRow, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
    use tempfile::tempdir;

    use crate::SqliteBackend;

    async fn make_api() -> (tempfile::TempDir, SqliteBackend) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("localdb.db");
        let backend = SqliteBackend::open(StoreBackendConfig::local_path(
            path,
            4,
            VectorEncoding::Float32,
        ))
        .await
        .unwrap();
        (dir, backend)
    }

    fn make_store(id: &str, name: &str) -> StoreRow {
        StoreRow {
            id: id.to_string(),
            name: name.to_string(),
            visibility: StoreVisibility::Private,
            backend: "libsql".to_string(),
            indexing_policy: "{}".to_string(),
            policy_version: "v1".to_string(),
            acl: "{}".to_string(),
            created_at: "2026-06-25T12:00:00Z".to_string(),
        }
    }

    fn make_source(id: &str, store_id: &str) -> SourceRow {
        SourceRow {
            id: id.to_string(),
            store_id: store_id.to_string(),
            kind: SourceKind::Path,
            root: Some("/docs".to_string()),
            url: None,
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: "2026-06-25T12:00:00Z".to_string(),
            config_json: None,
        }
    }

    fn make_chunk(id: &str, store_id: &str) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            resource_id: "doc-1".to_string(),
            store_id: store_id.to_string(),
            text: "some chunk text".to_string(),
            span: Span::new(0, 16),
            heading_path: vec![],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "v1".to_string(),
            fetched_at: "2026-07-01T00:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: "file:///docs/doc.md".to_string(),
            metadata: Default::default(),
            block_seq: 0,
            seq_in_block: 0,
            block_kind: None,
            page: None,
            window_block_seqs: vec![],
        }
    }

    #[tokio::test]
    async fn largest_tables_does_not_error_on_a_fresh_db() {
        let (_dir, api) = make_api().await;
        // A freshly created schema already has pages (empty tables + their
        // indexes) — this just pins that `dbstat` works at all through
        // libsql and the query never errors out.
        let tables = api.largest_tables(5).await.unwrap();
        assert!(
            tables.iter().all(|t| t.bytes > 0),
            "every reported table must have a positive byte size: {tables:?}"
        );
    }

    #[tokio::test]
    async fn largest_tables_reports_chunks_after_a_write() {
        let (_dir, api) = make_api().await;
        api.upsert_store(&make_store("store-1", "notes"))
            .await
            .unwrap();
        api.upsert_source(&make_source("src-1", "store-1"))
            .await
            .unwrap();
        let handle = api.retrieval_store("store-1").await.unwrap();
        handle
            .upsert_chunks(vec![make_chunk("c1", "store-1")])
            .await
            .unwrap();

        let tables = api.largest_tables(10).await.unwrap();
        assert!(
            tables.iter().any(|t| t.name == "chunks"),
            "expected a 'chunks' row among {tables:?}"
        );
    }

    #[tokio::test]
    async fn largest_tables_respects_the_limit() {
        let (_dir, api) = make_api().await;
        let tables = api.largest_tables(1).await.unwrap();
        assert!(
            tables.len() <= 1,
            "got {} rows, expected <= 1",
            tables.len()
        );
    }

    #[tokio::test]
    async fn largest_tables_is_sorted_descending_by_size() {
        let (_dir, api) = make_api().await;
        api.upsert_store(&make_store("store-1", "notes"))
            .await
            .unwrap();
        api.upsert_source(&make_source("src-1", "store-1"))
            .await
            .unwrap();
        let handle = api.retrieval_store("store-1").await.unwrap();
        handle
            .upsert_chunks(vec![make_chunk("c1", "store-1")])
            .await
            .unwrap();

        let tables = api.largest_tables(20).await.unwrap();
        let sizes: Vec<u64> = tables.iter().map(|t| t.bytes).collect();
        let mut sorted = sizes.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted, "rows must be sorted descending by bytes");
    }
}
