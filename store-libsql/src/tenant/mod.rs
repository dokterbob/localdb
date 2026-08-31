use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::ingestion::DocumentRecord;
use localdb_core::{
    ChunkRecord, Error, MetadataFilter, ResourceRecord, RetrievalStore, SearchResult, StoreStats,
    VectorEncoding,
};

use crate::connection::LibsqlDb;

pub(crate) mod read;
pub(crate) mod rows;
pub(crate) mod sql;
pub(crate) mod write;

#[cfg(test)]
mod tests;

pub(crate) struct TenantStore {
    conn: Arc<LibsqlDb>,
    store_id: String,
    embedding_dim: usize,
    encoding: VectorEncoding,
}

impl TenantStore {
    pub(crate) fn new(
        conn: Arc<LibsqlDb>,
        store_id: String,
        embedding_dim: usize,
        encoding: VectorEncoding,
    ) -> Self {
        Self {
            conn,
            store_id,
            embedding_dim,
            encoding,
        }
    }

    pub(crate) fn store_id(&self) -> &str {
        &self.store_id
    }

    pub(crate) fn conn(&self) -> &Arc<LibsqlDb> {
        &self.conn
    }

    pub(crate) fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    pub(crate) fn encoding(&self) -> VectorEncoding {
        self.encoding
    }
}

#[async_trait]
impl RetrievalStore for TenantStore {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        write::upsert_chunks(self, records).await
    }

    async fn delete_by_resource(&self, resource_id: &str) -> Result<usize, Error> {
        write::delete_by_resource(self, resource_id).await
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        write::delete_by_store(self, store_id).await
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        read::dense_search(self, query_vector, limit, filters).await
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        read::bm25_search(self, query_text, limit, filters).await
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        read::stats(self).await
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        read::get_chunk(self, chunk_id).await
    }

    async fn get_chunks_for_resource(&self, resource_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        read::get_chunks_for_resource(self, resource_id).await
    }

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        read::list_indexed_documents(self).await
    }

    async fn update_resource_metadata(
        &self,
        store_id: &str,
        resource_id: &str,
        record: &ResourceRecord,
    ) -> Result<(), Error> {
        write::update_resource_metadata(self, store_id, resource_id, record).await
    }

    async fn upsert_blocks(
        &self,
        _store_id: &str,
        resource_id: &str,
        blocks: &[localdb_core::block::Block],
    ) -> Result<(), localdb_core::Error> {
        write::upsert_blocks(self, resource_id, blocks).await
    }

    async fn get_blocks_for_resource(
        &self,
        resource_id: &str,
    ) -> Result<Vec<localdb_core::block::Block>, Error> {
        read::get_blocks_for_resource(self, resource_id).await
    }

    async fn upsert_chunks_and_blocks(
        &self,
        _store_id: &str,
        resource_id: &str,
        records: Vec<localdb_core::ChunkRecord>,
        blocks: &[localdb_core::block::Block],
        replaces_resource_id: Option<&str>,
        external_last_modified: Option<&str>,
    ) -> Result<usize, localdb_core::Error> {
        write::upsert_chunks_and_blocks(
            self,
            resource_id,
            records,
            blocks,
            replaces_resource_id,
            external_last_modified,
        )
        .await
    }
}
