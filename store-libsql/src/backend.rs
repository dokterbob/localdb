use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::{
    DocumentInfo, Error, RetrievalStore, SourceRow, StoreBackend, StoreBackendConfig,
    StoreBackendConnection, StoreRow, VectorEncoding,
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

    async fn find_document(&self, doc_id: &str) -> Result<Option<DocumentInfo>, Error> {
        registry::documents::find_document(&self.conn, doc_id).await
    }

    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        Ok(Arc::new(TenantStore::new(
            self.conn.clone(),
            store_id.to_string(),
            self.embedding_dim,
            self.encoding,
        )))
    }
}
