use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::parser::DocumentMetadata;
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentInfo {
    pub store_id: String,
    pub id: String,
    pub source_id: String,
    pub source_kind: String,
    pub uri: String,
    pub title: Option<String>,
    pub mime: Option<String>,
    pub content_hash: String,
    pub fetched_at: String,
    pub origin_store: String,
    pub policy_version: String,
    pub metadata: DocumentMetadata,
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

    async fn find_document(&self, doc_id: &str) -> Result<Option<DocumentInfo>, Error>;

    async fn retrieval_store(&self, store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error>;
}
