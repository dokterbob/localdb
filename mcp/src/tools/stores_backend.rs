//! Test double: `StoresBackend` — a `StoreBackend` derived on demand from a
//! slice of `AvailableStore`s.

use std::sync::Arc;

use async_trait::async_trait;

use localdb_core::{
    store::RetrievalStore, DocumentInfo, Error, SourceRow, StoreBackend, StoreBackendConfig,
    StoreRow, TableSize,
};

use super::AvailableStore;

/// A `StoreBackend` whose `find_document`/`retrieval_store` are derived on
/// demand from a `Vec<AvailableStore>`, rather than from an independently
/// maintained document registry.
///
/// Plain `pub`, like `localdb_core::store::FakeStore` — not `#[cfg(test)]`
/// gated — so both this crate's own unit tests (`tools/tests/*.rs`) and its
/// external integration tests (`mcp/tests/*.rs`) can build a `StoreBackend`
/// straight from the `AvailableStore` fixtures they already construct,
/// without maintaining a second, parallel document registry by hand. It has
/// no legitimate production use: real callers (`cli`, `server`) always pass
/// the real backend the `AvailableStore` handles were themselves resolved
/// from.
///
/// `find_document` mirrors the brute-force scan `get_document`'s tool body
/// used before `StoreBackend` threading: fetch a candidate store's chunks
/// for the id and treat the first chunk as the document's registry row,
/// discarding a store whose chunk's own `store_id` doesn't match the store
/// being queried (a federated/mismatched-data guard). `list_documents` and
/// every store/source-registry method are `unimplemented!()` — this double
/// only backs `get_document_detail`'s two calls, `find_document` and
/// `retrieval_store`.
pub struct StoresBackend {
    stores: Vec<AvailableStore>,
}

impl StoresBackend {
    /// Build a `StoresBackend` over a snapshot of `stores`.
    pub fn new(stores: &[AvailableStore]) -> Self {
        Self {
            stores: stores.to_vec(),
        }
    }
}

#[async_trait]
impl StoreBackend for StoresBackend {
    async fn open(_config: StoreBackendConfig) -> Result<Self, Error>
    where
        Self: Sized,
    {
        unimplemented!("test-only backend never constructed via open()")
    }

    async fn upsert_store(&self, _store: &StoreRow) -> Result<(), Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn delete_store(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn get_store(&self, _id: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn get_store_by_name(&self, _name: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn upsert_source(&self, _source: &SourceRow) -> Result<(), Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn delete_source(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn get_source(&self, _id: &str) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn list_sources(&self, _store_id: &str) -> Result<Vec<SourceRow>, Error> {
        unimplemented!("not exercised via get_document")
    }
    async fn find_source_by_root_or_url(
        &self,
        _value: &str,
        _store_id: &str,
    ) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised via get_document")
    }

    async fn find_document(
        &self,
        doc_id: &str,
        store_id: Option<&str>,
    ) -> Result<Option<DocumentInfo>, Error> {
        let scoped: Vec<&AvailableStore> = match store_id {
            Some(id) => self
                .stores
                .iter()
                .filter(|s| s.descriptor.id == id)
                .collect(),
            None => self.stores.iter().collect(),
        };

        for store in scoped {
            let chunks = store.store.get_chunks_for_resource(doc_id).await?;
            let Some(first) = chunks.first() else {
                continue;
            };
            if first.store_id != store.descriptor.id {
                continue;
            }
            return Ok(Some(DocumentInfo {
                store_id: first.store_id.clone(),
                id: first.resource_id.clone(),
                source_id: first.source_id.clone(),
                ingestor_kind: first.ingestor_kind.clone(),
                uri: first.uri.clone(),
                title: first.metadata.title().map(|t| t.to_string()),
                mime: first.mime.clone(),
                content_hash: first.content_hash.clone(),
                fetched_at: first.fetched_at.clone(),
                origin_store: first.origin_store.clone(),
                policy_version: first.policy_version.clone(),
                metadata: first.metadata.clone(),
            }));
        }
        Ok(None)
    }

    async fn list_documents(
        &self,
        _store_id: &str,
        _source_id: Option<&str>,
    ) -> Result<Vec<DocumentInfo>, Error> {
        unimplemented!(
            "not exercised via get_document; see tools/tests/list_documents.rs's own backend"
        )
    }

    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        self.stores
            .iter()
            .find(|s| s.descriptor.id == store_id)
            .map(|s| Arc::clone(&s.store))
            .ok_or_else(|| Error::StoreNotFound {
                id: store_id.to_string(),
            })
    }

    async fn largest_tables(&self, _limit: usize) -> Result<Vec<TableSize>, Error> {
        Ok(Vec::new())
    }
}
