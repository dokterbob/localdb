use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::{
    DocumentInfo, Error, RetrievalStore, SourceRow, StoreBackend, StoreBackendConfig,
    StoreBackendConnection, StoreRow, TableSize, VectorEncoding,
};

use crate::connection::LibsqlDb;
use crate::registry;
use crate::tenant::TenantStore;

pub struct SqliteBackend {
    pub(crate) conn: Arc<LibsqlDb>,
    embedding_dim: usize,
    encoding: VectorEncoding,
}

#[async_trait]
impl StoreBackend for SqliteBackend {
    async fn open(config: StoreBackendConfig) -> Result<Self, Error> {
        let path = match config.connection {
            StoreBackendConnection::LocalPath(path) => path,
            StoreBackendConnection::Url(_) => {
                return Err(Error::InvalidConfig {
                    message: "remote backend connections are not yet supported".to_string(),
                });
            }
        };
        let conn = Arc::new(LibsqlDb::open(&path, config.embedding_dim, config.encoding).await?);
        Ok(Self {
            conn,
            embedding_dim: config.embedding_dim,
            encoding: config.encoding,
        })
    }

    async fn upsert_store(&self, store: &StoreRow) -> Result<(), Error> {
        registry::stores::upsert_store(&self.conn, store).await
    }

    async fn delete_store(&self, id: &str) -> Result<bool, Error> {
        registry::stores::delete_store(&self.conn, id).await
    }

    async fn get_store(&self, id: &str) -> Result<Option<StoreRow>, Error> {
        registry::stores::get_store(&self.conn, id).await
    }

    async fn get_store_by_name(&self, name: &str) -> Result<Option<StoreRow>, Error> {
        registry::stores::get_store_by_name(&self.conn, name).await
    }

    async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
        registry::stores::list_stores(&self.conn).await
    }

    async fn upsert_source(&self, source: &SourceRow) -> Result<(), Error> {
        registry::sources::upsert_source(&self.conn, source).await
    }

    async fn delete_source(&self, id: &str) -> Result<bool, Error> {
        registry::sources::delete_source(&self.conn, id).await
    }

    async fn get_source(&self, id: &str) -> Result<Option<SourceRow>, Error> {
        registry::sources::get_source(&self.conn, id).await
    }

    async fn list_sources(&self, store_id: &str) -> Result<Vec<SourceRow>, Error> {
        registry::sources::list_sources(&self.conn, store_id).await
    }

    async fn find_source_by_root_or_url(
        &self,
        value: &str,
        store_id: &str,
    ) -> Result<Option<SourceRow>, Error> {
        registry::sources::find_source_by_root_or_url(&self.conn, value, store_id).await
    }

    async fn update_source_feed_cache(
        &self,
        id: &str,
        feed_etag: Option<&str>,
        feed_last_modified: Option<&str>,
        feed_inputs_digest: Option<&str>,
    ) -> Result<bool, Error> {
        registry::sources::update_source_feed_cache(
            &self.conn,
            id,
            feed_etag,
            feed_last_modified,
            feed_inputs_digest,
        )
        .await
    }

    async fn find_document(
        &self,
        doc_id: &str,
        store_id: Option<&str>,
    ) -> Result<Option<DocumentInfo>, Error> {
        registry::documents::find_document(&self.conn, doc_id, store_id).await
    }

    async fn list_documents(
        &self,
        store_id: &str,
        source_id: Option<&str>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Vec<DocumentInfo>, Error> {
        registry::documents::list_documents(&self.conn, store_id, source_id, limit, offset).await
    }

    async fn count_documents(&self, store_id: &str, source_id: Option<&str>) -> Result<u64, Error> {
        registry::documents::count_documents(&self.conn, store_id, source_id).await
    }

    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        Ok(Arc::new(TenantStore::new(
            self.conn.clone(),
            store_id.to_string(),
            self.embedding_dim,
            self.encoding,
        )))
    }

    async fn largest_tables(&self, limit: usize) -> Result<Vec<TableSize>, Error> {
        registry::diagnostics::largest_tables(&self.conn, limit).await
    }
}

/// Test-only escape hatch for backdating `resources.last_checked_at`
/// directly, bypassing `RetrievalStore::touch_resource_checked`'s "advance to
/// now" contract.
///
/// `touch_resource_checked`/`touch_resource_liveness` only ever move this
/// column forward to the current time — by design, there is no store-API way
/// to set it to an arbitrary past value. A test that needs to simulate a
/// resource whose last successful check happened long enough ago to clear
/// the recheck floor (specs/04-search-pipeline.md §1 "Recheck gate") has
/// nothing else to reach for. `store-libsql`'s own tests reach the private
/// `conn` field directly (same crate); this exists for integration tests in
/// other crates — `server/src/job_exec/tests/feed_liveness_sweep.rs` — that
/// can't.
#[cfg(any(test, feature = "test-support"))]
impl SqliteBackend {
    /// Overwrite `resources.last_checked_at` for one resource. `value` is an
    /// RFC 3339 timestamp string, or `None` to clear it back to `NULL`.
    pub async fn set_last_checked_at_for_test(
        &self,
        store_id: &str,
        resource_id: &str,
        value: Option<&str>,
    ) -> Result<(), Error> {
        let conn = self.conn.writer().await;
        conn.execute(
            "UPDATE resources SET last_checked_at = ?1 WHERE store_id = ?2 AND id = ?3",
            libsql::params![value, store_id, resource_id],
        )
        .await
        .map_err(crate::connection::map_libsql_err)?;
        Ok(())
    }
}
