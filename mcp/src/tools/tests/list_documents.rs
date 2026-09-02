//! `tool_list_documents` tests: store resolution by id/name, store_not_found,
//! source filter pass-through, offset/limit slicing, response shape.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use localdb_core::{
    backend::{SourceRow, StoreBackendConfig, StoreRow, TableSize},
    metadata::Metadata,
    store::{FakeStore, RetrievalStore},
    DocumentInfo, Error, StoreBackend,
};

use crate::args::ListDocumentsArgs;
use crate::tools::{tool_list_documents, AvailableStore};

use super::common::make_descriptor;

// ---------------------------------------------------------------------------
// A minimal fake `StoreBackend`, modeled on `core/src/documents/tests.rs`'s
// own `FakeBackend` — only `list_documents` carries real behavior, since
// that's the sole method `tool_list_documents` calls.
// ---------------------------------------------------------------------------

struct FakeBackend {
    /// store_id -> every document registered in that store.
    documents: HashMap<String, Vec<DocumentInfo>>,
}

impl FakeBackend {
    fn new() -> Self {
        Self {
            documents: HashMap::new(),
        }
    }

    fn with_documents(mut self, store_id: &str, docs: Vec<DocumentInfo>) -> Self {
        self.documents.insert(store_id.to_string(), docs);
        self
    }
}

#[async_trait]
impl StoreBackend for FakeBackend {
    async fn open(_config: StoreBackendConfig) -> Result<Self, Error>
    where
        Self: Sized,
    {
        unimplemented!("never constructed via the trait's own open()")
    }

    async fn upsert_store(&self, _store: &StoreRow) -> Result<(), Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn delete_store(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn get_store(&self, _id: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn get_store_by_name(&self, _name: &str) -> Result<Option<StoreRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn list_stores(&self) -> Result<Vec<StoreRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn upsert_source(&self, _source: &SourceRow) -> Result<(), Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn delete_source(&self, _id: &str) -> Result<bool, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn get_source(&self, _id: &str) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn list_sources(&self, _store_id: &str) -> Result<Vec<SourceRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn find_source_by_root_or_url(
        &self,
        _value: &str,
        _store_id: &str,
    ) -> Result<Option<SourceRow>, Error> {
        unimplemented!("not exercised by list_documents")
    }

    async fn update_source_feed_cache(
        &self,
        _id: &str,
        _feed_etag: Option<&str>,
        _feed_last_modified: Option<&str>,
        _feed_inputs_digest: Option<&str>,
    ) -> Result<bool, Error> {
        unimplemented!("not exercised by list_documents")
    }
    async fn find_document(
        &self,
        _doc_id: &str,
        _store_id: Option<&str>,
    ) -> Result<Option<DocumentInfo>, Error> {
        unimplemented!("not exercised by list_documents")
    }

    async fn list_documents(
        &self,
        store_id: &str,
        source_id: Option<&str>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Vec<DocumentInfo>, Error> {
        let docs = self.documents.get(store_id).cloned().unwrap_or_default();
        let filtered = docs
            .into_iter()
            .filter(|d| source_id.map(|s| d.source_id == s).unwrap_or(true));
        Ok(match limit {
            Some(limit) => filtered.skip(offset).take(limit).collect(),
            None => filtered.skip(offset).collect(),
        })
    }

    async fn count_documents(&self, store_id: &str, source_id: Option<&str>) -> Result<u64, Error> {
        let docs = self.documents.get(store_id).cloned().unwrap_or_default();
        let count = docs
            .into_iter()
            .filter(|d| source_id.map(|s| d.source_id == s).unwrap_or(true))
            .count();
        Ok(count as u64)
    }

    async fn retrieval_store(&self, _store_id: &str) -> Result<Arc<dyn RetrievalStore>, Error> {
        unimplemented!("not exercised by list_documents")
    }

    async fn largest_tables(&self, _limit: usize) -> Result<Vec<TableSize>, Error> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn make_doc(id: &str, store_id: &str, source_id: &str, uri: &str) -> DocumentInfo {
    DocumentInfo {
        store_id: store_id.to_string(),
        id: id.to_string(),
        source_id: source_id.to_string(),
        ingestor_kind: "path".to_string(),
        uri: uri.to_string(),
        title: None,
        mime: None,
        content_hash: "hash".to_string(),
        fetched_at: "2026-01-01T00:00:00Z".to_string(),
        origin_store: store_id.to_string(),
        policy_version: "v1".to_string(),
        metadata: Metadata::default(),
        date_original: None,
        date_parsed: None,
        index_updated_at: None,
    }
}

/// `tool_list_documents` never touches `AvailableStore::store` — only
/// `descriptor.id`/`descriptor.name` (for `select_mcp_stores`) — so an empty
/// `FakeStore` stands in for the session-scoped `RetrievalStore` handle.
fn available_store(id: &str, name: &str) -> AvailableStore {
    AvailableStore::new(make_descriptor(id, name), Box::new(FakeStore::new()))
}

fn list_args(store: &str) -> ListDocumentsArgs {
    ListDocumentsArgs {
        store: store.to_string(),
        source: None,
        offset: None,
        limit: None,
    }
}

// ---------------------------------------------------------------------------
// Store resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolves_store_by_id() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new().with_documents(
        "store-1",
        vec![make_doc("doc-1", "store-1", "src-1", "file:///a.md")],
    );

    let result = tool_list_documents(&stores, &backend, list_args("store-1")).await;
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["store"]["id"], "store-1");
    assert_eq!(parsed["store"]["name"], "notes");
    assert_eq!(parsed["documents"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn resolves_store_by_name() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new().with_documents(
        "store-1",
        vec![make_doc("doc-1", "store-1", "src-1", "file:///a.md")],
    );

    let result = tool_list_documents(&stores, &backend, list_args("notes")).await;
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["store"]["id"], "store-1");
    assert_eq!(parsed["store"]["name"], "notes");
}

#[tokio::test]
async fn unknown_store_returns_store_not_found() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new();

    let result = tool_list_documents(&stores, &backend, list_args("no-such-store")).await;
    assert_eq!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"], "store_not_found");
}

// ---------------------------------------------------------------------------
// Source filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn source_filter_passes_through_to_backend() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new().with_documents(
        "store-1",
        vec![
            make_doc("doc-1", "store-1", "src-a", "file:///a.md"),
            make_doc("doc-2", "store-1", "src-b", "file:///b.md"),
        ],
    );

    let mut args = list_args("store-1");
    args.source = Some("src-a".to_string());
    let result = tool_list_documents(&stores, &backend, args).await;
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let docs = parsed["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["id"], "doc-1");
}

#[tokio::test]
async fn unknown_source_yields_empty_list_not_error() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new().with_documents(
        "store-1",
        vec![make_doc("doc-1", "store-1", "src-a", "file:///a.md")],
    );

    let mut args = list_args("store-1");
    args.source = Some("no-such-source".to_string());
    let result = tool_list_documents(&stores, &backend, args).await;
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["documents"].as_array().unwrap().len(), 0);
    assert_eq!(parsed["total"], 0);
}

// ---------------------------------------------------------------------------
// offset/limit slicing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn offset_and_limit_slice_the_document_list() {
    let stores = vec![available_store("store-1", "notes")];
    let docs: Vec<DocumentInfo> = (0..5)
        .map(|i| {
            make_doc(
                &format!("doc-{i}"),
                "store-1",
                "src-1",
                &format!("file:///{i}.md"),
            )
        })
        .collect();
    let backend = FakeBackend::new().with_documents("store-1", docs);

    let mut args = list_args("store-1");
    args.offset = Some(1);
    args.limit = Some(2);
    let result = tool_list_documents(&stores, &backend, args).await;
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["total"], 5);
    assert_eq!(parsed["offset"], 1);
    assert_eq!(parsed["limit"], 2);
    assert_eq!(parsed["returned"], 2);
    let docs = parsed["documents"].as_array().unwrap();
    assert_eq!(docs.len(), 2);
    assert_eq!(docs[0]["id"], "doc-1");
    assert_eq!(docs[1]["id"], "doc-2");
}

#[tokio::test]
async fn offset_past_end_yields_empty_page_not_error() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new().with_documents(
        "store-1",
        vec![make_doc("doc-1", "store-1", "src-1", "file:///a.md")],
    );

    let mut args = list_args("store-1");
    args.offset = Some(100);
    let result = tool_list_documents(&stores, &backend, args).await;
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["total"], 1);
    assert_eq!(parsed["returned"], 0);
    assert_eq!(parsed["documents"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn explicit_zero_limit_is_a_typed_error() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new();

    let mut args = list_args("store-1");
    args.limit = Some(0);
    let result = tool_list_documents(&stores, &backend, args).await;
    assert_eq!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"], "invalid_request");
}

// ---------------------------------------------------------------------------
// Response shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn response_shape_carries_serialized_document_info() {
    let stores = vec![available_store("store-1", "notes")];
    let backend = FakeBackend::new().with_documents(
        "store-1",
        vec![make_doc("doc-1", "store-1", "src-1", "file:///a.md")],
    );

    let result = tool_list_documents(&stores, &backend, list_args("store-1")).await;
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

    assert_eq!(
        parsed["store"],
        serde_json::json!({ "id": "store-1", "name": "notes" })
    );
    assert_eq!(parsed["total"], 1);
    assert_eq!(parsed["offset"], 0);
    assert_eq!(
        parsed["limit"], 50,
        "default limit matches get_chunks's default"
    );
    assert_eq!(parsed["returned"], 1);

    let doc = &parsed["documents"][0];
    assert_eq!(doc["store_id"], "store-1");
    assert_eq!(doc["id"], "doc-1");
    assert_eq!(doc["source_id"], "src-1");
    assert_eq!(doc["uri"], "file:///a.md");
    assert_eq!(doc["content_hash"], "hash");
}
