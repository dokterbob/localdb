//! LanceDB implementation of the `RetrievalStore` trait.
//!
//! Provides embedded LanceDB with tantivy BM25 for hybrid search.
//!
//! # Design
//!
//! One LanceDB table per logical store, stored under `{data_dir}/{store_id}/chunks`.
//! The table schema mirrors `ChunkRecord` with one additional column: `embedding`
//! as a `FixedSizeList<Float32>`.
//!
//! BM25 full-text search uses LanceDB's built-in FTS index (tantivy underneath).
//! Dense search uses LanceDB's IVF-PQ or exact KNN (auto-selected based on row count).
//!
//! # Fusion
//!
//! RRF fusion is NOT done here — the trait exposes raw ranked result lists.
//! Fusion lives in `core` (T08).

use std::collections::HashMap;
use std::sync::Arc;

use arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
    UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use async_trait::async_trait;
use futures::TryStreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::index::{scalar::BTreeIndexBuilder, Index};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, Table};

use localdb_core::store::{ChunkRecord, MetadataFilter, RetrievalStore, SearchResult, StoreStats};
use localdb_core::types::Span;
use localdb_core::Error;

// ---------------------------------------------------------------------------
// Schema constants
// ---------------------------------------------------------------------------

/// The LanceDB table name used by all stores (one DB per store directory).
const TABLE_NAME: &str = "chunks";

/// Column names
const COL_ID: &str = "id";
const COL_DOCUMENT_ID: &str = "document_id";
const COL_STORE_ID: &str = "store_id";
const COL_TEXT: &str = "text";
const COL_SPAN_START: &str = "span_start";
const COL_SPAN_END: &str = "span_end";
const COL_HEADING_PATH: &str = "heading_path";
const COL_EMBEDDING: &str = "embedding";
const COL_POLICY_VERSION: &str = "policy_version";
const COL_FETCHED_AT: &str = "fetched_at";
const COL_CONTENT_HASH: &str = "content_hash";
const COL_ORIGIN_STORE: &str = "origin_store";
const COL_SOURCE_ID: &str = "source_id";
const COL_SOURCE_KIND: &str = "source_kind";
const COL_MIME: &str = "mime";
const COL_URI: &str = "uri";
const COL_TITLE: &str = "title";

/// The special LanceDB `_distance` column added to vector search results.
const COL_DISTANCE: &str = "_distance";

/// The special LanceDB `_score` column added to FTS results.
const COL_SCORE: &str = "_score";

// ---------------------------------------------------------------------------
// LanceDbStore
// ---------------------------------------------------------------------------

/// LanceDB-backed implementation of `RetrievalStore`.
///
/// Each instance represents one logical store backed by a LanceDB table.
/// The `embedding_dim` must match the dimension of vectors stored in the table.
pub struct LanceDbStore {
    table: Table,
    embedding_dim: usize,
}

impl LanceDbStore {
    /// Open or create a LanceDB store at the given path.
    ///
    /// # Arguments
    /// * `path` - Directory path for this store's LanceDB database.
    /// * `embedding_dim` - Dimension of the embedding vectors. Must be consistent.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or the table cannot be created.
    pub async fn open(path: &str, embedding_dim: usize) -> Result<Self, Error> {
        let db = connect(path).execute().await.map_err(|e| Error::Internal {
            message: format!("LanceDB connect failed: {e}"),
            correlation_id: "lancedb-open".to_string(),
        })?;

        // Check if table exists
        let table_names = db
            .table_names()
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB list tables failed: {e}"),
                correlation_id: "lancedb-list".to_string(),
            })?;

        let table = if table_names.contains(&TABLE_NAME.to_string()) {
            let table = db
                .open_table(TABLE_NAME)
                .execute()
                .await
                .map_err(|e| Error::Internal {
                    message: format!("LanceDB open table failed: {e}"),
                    correlation_id: "lancedb-open-table".to_string(),
                })?;

            // A5: validate embedding dim matches what is stored in the table schema.
            let schema = table.schema().await.map_err(|e| Error::Internal {
                message: format!("LanceDB schema read failed: {e}"),
                correlation_id: "lancedb-schema".to_string(),
            })?;
            let stored_dim = schema.field_with_name(COL_EMBEDDING).ok().and_then(|f| {
                if let DataType::FixedSizeList(_, dim) = f.data_type() {
                    Some(*dim as usize)
                } else {
                    None
                }
            });
            if let Some(dim) = stored_dim {
                if dim != embedding_dim {
                    return Err(Error::InvalidConfig {
                        message: format!(
                            "embedding dimension mismatch: store at '{path}' was created with \
                             dim={dim} but the current embedder produces dim={embedding_dim}. \
                             Update your embedding config to match the stored dimension, or \
                             delete the store and re-run `localdb index` to rebuild it.",
                        ),
                    });
                }
            }
            table
        } else {
            // Create the table with the schema
            let schema = make_schema(embedding_dim);
            // Create an empty batch to initialize the table
            let empty_batch = make_empty_batch(&schema);
            let reader = RecordBatchIterator::new(vec![Ok(empty_batch)], Arc::clone(&schema));
            db.create_table(TABLE_NAME, Box::new(reader))
                .execute()
                .await
                .map_err(|e| Error::Internal {
                    message: format!("LanceDB create table failed: {e}"),
                    correlation_id: "lancedb-create-table".to_string(),
                })?
        };

        Ok(Self {
            table,
            embedding_dim,
        })
    }

    /// Create a full-text search index on the `text` column.
    ///
    /// Should be called after initial bulk ingest and periodically thereafter.
    /// Safe to call multiple times (it's idempotent in LanceDB).
    pub async fn create_fts_index(&self) -> Result<(), Error> {
        self.table
            .create_index(&[COL_TEXT], Index::FTS(Default::default()))
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("FTS index creation failed: {e}"),
                correlation_id: "lancedb-fts-index".to_string(),
            })
    }

    /// Create a scalar index on `document_id` for fast deletion by document.
    pub async fn create_document_id_index(&self) -> Result<(), Error> {
        self.table
            .create_index(
                &[COL_DOCUMENT_ID],
                Index::BTree(BTreeIndexBuilder::default()),
            )
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("BTree index creation failed: {e}"),
                correlation_id: "lancedb-btree-index".to_string(),
            })
    }
}

// ---------------------------------------------------------------------------
// Schema and batch construction helpers
// ---------------------------------------------------------------------------

/// Build the Arrow schema for the chunks table.
fn make_schema(embedding_dim: usize) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new(COL_ID, DataType::Utf8, false),
        Field::new(COL_DOCUMENT_ID, DataType::Utf8, false),
        Field::new(COL_STORE_ID, DataType::Utf8, false),
        Field::new(COL_TEXT, DataType::Utf8, false),
        Field::new(COL_SPAN_START, DataType::UInt64, false),
        Field::new(COL_SPAN_END, DataType::UInt64, false),
        Field::new(COL_HEADING_PATH, DataType::Utf8, false), // JSON-encoded
        Field::new(
            COL_EMBEDDING,
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                embedding_dim as i32,
            ),
            false,
        ),
        Field::new(COL_POLICY_VERSION, DataType::Utf8, false),
        Field::new(COL_FETCHED_AT, DataType::Utf8, false),
        Field::new(COL_CONTENT_HASH, DataType::Utf8, false),
        Field::new(COL_ORIGIN_STORE, DataType::Utf8, false),
        Field::new(COL_SOURCE_ID, DataType::Utf8, false),
        Field::new(COL_SOURCE_KIND, DataType::Utf8, false),
        Field::new(COL_MIME, DataType::Utf8, true), // nullable
        Field::new(COL_URI, DataType::Utf8, false),
        Field::new(COL_TITLE, DataType::Utf8, true), // nullable
    ]))
}

/// Build an empty RecordBatch for table initialization.
fn make_empty_batch(schema: &Arc<Schema>) -> RecordBatch {
    let embedding_dim = match schema.field_with_name(COL_EMBEDDING).unwrap().data_type() {
        DataType::FixedSizeList(_, dim) => *dim as usize,
        _ => panic!("unexpected embedding type"),
    };
    RecordBatch::try_new(
        Arc::clone(schema),
        vec![
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(UInt64Array::from(Vec::<u64>::new())) as _,
            Arc::new(UInt64Array::from(Vec::<u64>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(FixedSizeListArray::from_iter_primitive::<
                arrow_array::types::Float32Type,
                _,
                _,
            >(
                std::iter::empty::<Option<Vec<Option<f32>>>>(),
                embedding_dim as i32,
            )) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<String>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<String>>::new())) as _,
        ],
    )
    .expect("empty batch construction failed")
}

/// Build a RecordBatch from a slice of ChunkRecords.
///
/// All records must have the same embedding dimension, and it must match `embedding_dim`.
fn records_to_batch(records: &[ChunkRecord], embedding_dim: usize) -> Result<RecordBatch, Error> {
    let schema = make_schema(embedding_dim);

    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    let doc_ids: Vec<&str> = records.iter().map(|r| r.document_id.as_str()).collect();
    let store_ids: Vec<&str> = records.iter().map(|r| r.store_id.as_str()).collect();
    let texts: Vec<&str> = records.iter().map(|r| r.text.as_str()).collect();
    let span_starts: Vec<u64> = records.iter().map(|r| r.span.start as u64).collect();
    let span_ends: Vec<u64> = records.iter().map(|r| r.span.end as u64).collect();
    let heading_paths: Vec<String> = records
        .iter()
        .map(|r| serde_json::to_string(&r.heading_path).unwrap_or_default())
        .collect();
    let policy_versions: Vec<&str> = records.iter().map(|r| r.policy_version.as_str()).collect();
    let fetched_ats: Vec<&str> = records.iter().map(|r| r.fetched_at.as_str()).collect();
    let content_hashes: Vec<&str> = records.iter().map(|r| r.content_hash.as_str()).collect();
    let origin_stores: Vec<&str> = records.iter().map(|r| r.origin_store.as_str()).collect();
    let source_ids: Vec<&str> = records.iter().map(|r| r.source_id.as_str()).collect();
    let source_kinds: Vec<&str> = records.iter().map(|r| r.source_kind.as_str()).collect();
    let mimes: Vec<Option<&str>> = records.iter().map(|r| r.mime.as_deref()).collect();
    let uris: Vec<&str> = records.iter().map(|r| r.uri.as_str()).collect();
    let titles: Vec<Option<&str>> = records.iter().map(|r| r.title.as_deref()).collect();

    // A4: validate all embedding dimensions match before building the batch.
    for (i, r) in records.iter().enumerate() {
        if r.embedding.len() != embedding_dim {
            return Err(Error::Internal {
                message: format!(
                    "embedding dimension mismatch at record {i}: expected {embedding_dim}, \
                     got {}. Ensure the embedder dimension matches the store schema.",
                    r.embedding.len()
                ),
                correlation_id: "dim-mismatch".to_string(),
            });
        }
    }

    // Build the FixedSizeList embedding column
    let embeddings = FixedSizeListArray::from_iter_primitive::<arrow_array::types::Float32Type, _, _>(
        records
            .iter()
            .map(|r| Some(r.embedding.iter().map(|&v| Some(v)).collect::<Vec<_>>())),
        embedding_dim as i32,
    );

    let heading_path_strs: Vec<&str> = heading_paths.iter().map(|s| s.as_str()).collect();

    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(StringArray::from(ids)) as _,
            Arc::new(StringArray::from(doc_ids)) as _,
            Arc::new(StringArray::from(store_ids)) as _,
            Arc::new(StringArray::from(texts)) as _,
            Arc::new(UInt64Array::from(span_starts)) as _,
            Arc::new(UInt64Array::from(span_ends)) as _,
            Arc::new(StringArray::from(heading_path_strs)) as _,
            Arc::new(embeddings) as _,
            Arc::new(StringArray::from(policy_versions)) as _,
            Arc::new(StringArray::from(fetched_ats)) as _,
            Arc::new(StringArray::from(content_hashes)) as _,
            Arc::new(StringArray::from(origin_stores)) as _,
            Arc::new(StringArray::from(source_ids)) as _,
            Arc::new(StringArray::from(source_kinds)) as _,
            Arc::new(StringArray::from(mimes)) as _,
            Arc::new(StringArray::from(uris)) as _,
            Arc::new(StringArray::from(titles)) as _,
        ],
    )
    .map_err(|e| Error::Internal {
        message: format!("RecordBatch construction failed: {e}"),
        correlation_id: "records-to-batch".to_string(),
    })?;

    Ok(batch)
}

// ---------------------------------------------------------------------------
// RecordBatch → ChunkRecord deserialization
// ---------------------------------------------------------------------------

/// Extract a string column value from a RecordBatch row.
fn get_str(batch: &RecordBatch, col: &str, row: usize) -> String {
    batch
        .column_by_name(col)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .map(|arr| {
            if arr.is_null(row) {
                String::new()
            } else {
                arr.value(row).to_string()
            }
        })
        .unwrap_or_default()
}

/// Extract an optional string column value.
fn get_opt_str(batch: &RecordBatch, col: &str, row: usize) -> Option<String> {
    batch
        .column_by_name(col)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .and_then(|arr| {
            if arr.is_null(row) {
                None
            } else {
                Some(arr.value(row).to_string())
            }
        })
}

/// Extract a u64 column value.
fn get_u64(batch: &RecordBatch, col: &str, row: usize) -> u64 {
    batch
        .column_by_name(col)
        .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
        .map(|arr| arr.value(row))
        .unwrap_or(0)
}

/// Extract the embedding vector for a given row.
fn get_embedding(batch: &RecordBatch, row: usize) -> Vec<f32> {
    let col = batch.column_by_name(COL_EMBEDDING);
    if let Some(col) = col {
        if let Some(list) = col.as_any().downcast_ref::<FixedSizeListArray>() {
            let values = list.value(row);
            if let Some(floats) = values.as_any().downcast_ref::<Float32Array>() {
                return (0..floats.len()).map(|i| floats.value(i)).collect();
            }
        }
    }
    vec![]
}

/// Get a float score from a named column (used for _distance and _score).
fn get_f32(batch: &RecordBatch, col: &str, row: usize) -> Option<f32> {
    batch
        .column_by_name(col)
        .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
        .map(|arr| arr.value(row))
}

/// Deserialize a row of a RecordBatch into a ChunkRecord.
fn row_to_chunk_record(batch: &RecordBatch, row: usize) -> ChunkRecord {
    let heading_path_json = get_str(batch, COL_HEADING_PATH, row);
    let heading_path: Vec<String> = serde_json::from_str(&heading_path_json).unwrap_or_default();

    ChunkRecord {
        id: get_str(batch, COL_ID, row),
        document_id: get_str(batch, COL_DOCUMENT_ID, row),
        store_id: get_str(batch, COL_STORE_ID, row),
        text: get_str(batch, COL_TEXT, row),
        span: Span::new(
            get_u64(batch, COL_SPAN_START, row) as usize,
            get_u64(batch, COL_SPAN_END, row) as usize,
        ),
        heading_path,
        embedding: get_embedding(batch, row),
        policy_version: get_str(batch, COL_POLICY_VERSION, row),
        fetched_at: get_str(batch, COL_FETCHED_AT, row),
        content_hash: get_str(batch, COL_CONTENT_HASH, row),
        origin_store: get_str(batch, COL_ORIGIN_STORE, row),
        source_id: get_str(batch, COL_SOURCE_ID, row),
        source_kind: get_str(batch, COL_SOURCE_KIND, row),
        mime: get_opt_str(batch, COL_MIME, row),
        uri: get_str(batch, COL_URI, row),
        title: get_opt_str(batch, COL_TITLE, row),
        meta: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Filter → SQL predicate
// ---------------------------------------------------------------------------

/// Build a SQL WHERE predicate from a slice of metadata filters.
///
/// Returns `None` if there are no filters (no WHERE clause needed).
fn filters_to_predicate(filters: &[MetadataFilter]) -> Option<String> {
    if filters.is_empty() {
        return None;
    }

    let clauses: Vec<String> = filters
        .iter()
        .map(|f| match f {
            MetadataFilter::Mime(mime) => format!("{COL_MIME} = '{}'", escape_sql(mime)),
            MetadataFilter::UriPrefix(prefix) => {
                format!("{COL_URI} LIKE '{}%'", escape_sql(prefix))
            }
            MetadataFilter::FetchedAfter(ts) => {
                format!("{COL_FETCHED_AT} >= '{}'", escape_sql(ts))
            }
            MetadataFilter::FetchedBefore(ts) => {
                format!("{COL_FETCHED_AT} <= '{}'", escape_sql(ts))
            }
            MetadataFilter::SourceId(id) => {
                format!("{COL_SOURCE_ID} = '{}'", escape_sql(id))
            }
            MetadataFilter::DocumentId(id) => {
                format!("{COL_DOCUMENT_ID} = '{}'", escape_sql(id))
            }
            MetadataFilter::PolicyVersion(v) => {
                format!("{COL_POLICY_VERSION} = '{}'", escape_sql(v))
            }
        })
        .collect();

    Some(clauses.join(" AND "))
}

/// Minimally escape single quotes for SQL string literals.
fn escape_sql(s: &str) -> String {
    s.replace('\'', "''")
}

// ---------------------------------------------------------------------------
// Result stream → Vec<SearchResult>
// ---------------------------------------------------------------------------

/// Collect a stream of RecordBatches into SearchResults, extracting the score column.
async fn collect_search_results(
    stream: impl futures::Stream<Item = Result<RecordBatch, lancedb::Error>>,
    score_col: &str,
    is_distance: bool,
) -> Result<Vec<SearchResult>, Error> {
    let batches: Vec<RecordBatch> = stream.try_collect().await.map_err(|e| Error::Internal {
        message: format!("stream collection failed: {e}"),
        correlation_id: "collect-results".to_string(),
    })?;

    let mut results = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            let record = row_to_chunk_record(batch, row);
            let raw_score = get_f32(batch, score_col, row).unwrap_or(0.0);
            // For dense search, LanceDB returns distance (lower = better).
            // Convert to similarity: score = 1 / (1 + distance)
            let score = if is_distance {
                1.0 / (1.0 + raw_score)
            } else {
                raw_score
            };
            results.push(SearchResult {
                chunk: record,
                score,
            });
        }
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// RetrievalStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl RetrievalStore for LanceDbStore {
    async fn upsert_chunks(&self, records: Vec<ChunkRecord>) -> Result<usize, Error> {
        if records.is_empty() {
            return Ok(0);
        }

        let count = records.len();

        // Build the record batch
        let batch = records_to_batch(&records, self.embedding_dim)?;
        let schema = Arc::clone(batch.schema_ref());

        // For each record, delete the existing chunk with the same id first,
        // then insert. This implements "upsert" semantics.
        //
        // Build delete predicate for all IDs in batch.
        let id_list: Vec<String> = records
            .iter()
            .map(|r| format!("'{}'", escape_sql(&r.id)))
            .collect();
        let delete_pred = format!("{COL_ID} IN ({})", id_list.join(", "));
        self.table
            .delete(&delete_pred)
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB delete before upsert failed: {e}"),
                correlation_id: "upsert-delete".to_string(),
            })?;

        // Now insert the new records
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        self.table
            .add(reader)
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB add failed: {e}"),
                correlation_id: "upsert-add".to_string(),
            })?;

        Ok(count)
    }

    async fn delete_by_document(&self, document_id: &str) -> Result<usize, Error> {
        // Count before delete
        let before = self
            .table
            .count_rows(Some(format!(
                "{COL_DOCUMENT_ID} = '{}'",
                escape_sql(document_id)
            )))
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB count failed: {e}"),
                correlation_id: "delete-doc-count".to_string(),
            })?;

        self.table
            .delete(&format!(
                "{COL_DOCUMENT_ID} = '{}'",
                escape_sql(document_id)
            ))
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB delete by document failed: {e}"),
                correlation_id: "delete-doc".to_string(),
            })?;

        Ok(before)
    }

    async fn delete_by_store(&self, store_id: &str) -> Result<usize, Error> {
        let before = self
            .table
            .count_rows(Some(format!("{COL_STORE_ID} = '{}'", escape_sql(store_id))))
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB count failed: {e}"),
                correlation_id: "delete-store-count".to_string(),
            })?;

        self.table
            .delete(&format!("{COL_STORE_ID} = '{}'", escape_sql(store_id)))
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB delete by store failed: {e}"),
                correlation_id: "delete-store".to_string(),
            })?;

        Ok(before)
    }

    async fn dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let mut query = self
            .table
            .query()
            .nearest_to(query_vector)
            .map_err(|e| Error::Internal {
                message: format!("LanceDB nearest_to failed: {e}"),
                correlation_id: "dense-search".to_string(),
            })?
            .limit(limit);

        if let Some(predicate) = filters_to_predicate(filters) {
            query = query.only_if(predicate);
        }

        let stream = query.execute().await.map_err(|e| Error::Internal {
            message: format!("LanceDB dense search execute failed: {e}"),
            correlation_id: "dense-execute".to_string(),
        })?;

        collect_search_results(stream, COL_DISTANCE, true).await
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let mut query = self
            .table
            .query()
            .full_text_search(FullTextSearchQuery::new(query_text.to_string()))
            .limit(limit);

        if let Some(predicate) = filters_to_predicate(filters) {
            query = query.only_if(predicate);
        }

        let stream = query.execute().await.map_err(|e| Error::Internal {
            message: format!("LanceDB BM25 search execute failed: {e}"),
            correlation_id: "bm25-execute".to_string(),
        })?;

        collect_search_results(stream, COL_SCORE, false).await
    }

    async fn stats(&self) -> Result<StoreStats, Error> {
        let chunk_count = self
            .table
            .count_rows(None)
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB count_rows failed: {e}"),
                correlation_id: "stats-count".to_string(),
            })? as u64;

        // Count distinct document IDs by querying and deduplicating
        let doc_count = if chunk_count == 0 {
            0
        } else {
            let stream = self
                .table
                .query()
                .select(lancedb::query::Select::columns(&[COL_DOCUMENT_ID]))
                .execute()
                .await
                .map_err(|e| Error::Internal {
                    message: format!("LanceDB stats query failed: {e}"),
                    correlation_id: "stats-docs".to_string(),
                })?;

            let batches: Vec<RecordBatch> =
                stream.try_collect().await.map_err(|e| Error::Internal {
                    message: format!("LanceDB stats stream failed: {e}"),
                    correlation_id: "stats-stream".to_string(),
                })?;

            let mut doc_ids = std::collections::HashSet::new();
            for batch in &batches {
                if let Some(col) = batch.column_by_name(COL_DOCUMENT_ID) {
                    if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                        for i in 0..arr.len() {
                            if !arr.is_null(i) {
                                doc_ids.insert(arr.value(i).to_string());
                            }
                        }
                    }
                }
            }
            doc_ids.len() as u64
        };

        Ok(StoreStats {
            chunk_count,
            document_count: doc_count,
        })
    }

    async fn get_chunk(&self, chunk_id: &str) -> Result<Option<ChunkRecord>, Error> {
        let stream = self
            .table
            .query()
            .only_if(format!("{COL_ID} = '{}'", escape_sql(chunk_id)))
            .limit(1)
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB get_chunk query failed: {e}"),
                correlation_id: "get-chunk".to_string(),
            })?;

        let batches: Vec<RecordBatch> =
            stream.try_collect().await.map_err(|e| Error::Internal {
                message: format!("LanceDB get_chunk stream failed: {e}"),
                correlation_id: "get-chunk-stream".to_string(),
            })?;

        for batch in &batches {
            if batch.num_rows() > 0 {
                return Ok(Some(row_to_chunk_record(batch, 0)));
            }
        }
        Ok(None)
    }

    async fn get_chunks_for_document(&self, document_id: &str) -> Result<Vec<ChunkRecord>, Error> {
        let stream = self
            .table
            .query()
            .only_if(format!("{COL_DOCUMENT_ID} = '{}'", escape_sql(document_id)))
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB get_chunks_for_document failed: {e}"),
                correlation_id: "get-doc-chunks".to_string(),
            })?;

        let batches: Vec<RecordBatch> =
            stream.try_collect().await.map_err(|e| Error::Internal {
                message: format!("LanceDB get_chunks_for_document stream failed: {e}"),
                correlation_id: "get-doc-chunks-stream".to_string(),
            })?;

        let mut records = Vec::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                records.push(row_to_chunk_record(batch, row));
            }
        }
        Ok(records)
    }
}

// ---------------------------------------------------------------------------
// Integration tests (run against real LanceDB with tmpdir)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::store::conformance;

    /// Embedding dimension for local tests (4D for better test coverage).
    const DIM: usize = 4;

    /// Embedding dimension that matches the conformance module's test records (2D).
    const CONFORMANCE_DIM: usize = 2;

    fn make_record(
        id: &str,
        doc_id: &str,
        store_id: &str,
        text: &str,
        embedding: Vec<f32>,
    ) -> ChunkRecord {
        ChunkRecord {
            id: id.to_string(),
            document_id: doc_id.to_string(),
            store_id: store_id.to_string(),
            text: text.to_string(),
            span: Span::new(0, text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            origin_store: store_id.to_string(),
            source_id: "src-1".to_string(),
            source_kind: "path".to_string(),
            mime: Some("text/plain".to_string()),
            uri: "file:///test.md".to_string(),
            title: Some("Test".to_string()),
            meta: HashMap::new(),
        }
    }

    /// Create a fresh store with DIM=4 (local tests).
    async fn fresh_store() -> (LanceDbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let store = LanceDbStore::open(&path, DIM).await.unwrap();
        (store, dir)
    }

    /// Create a fresh store with DIM=2 (conformance suite uses 2D vectors).
    async fn fresh_conformance_store() -> (LanceDbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let store = LanceDbStore::open(&path, CONFORMANCE_DIM).await.unwrap();
        (store, dir)
    }

    // --- Conformance suite against LanceDB ---
    // NOTE: conformance tests use 2D embeddings, so we use CONFORMANCE_DIM stores.

    #[tokio::test]
    async fn lancedb_upsert_and_stats() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_upsert_and_stats(&store).await;
    }

    #[tokio::test]
    async fn lancedb_upsert_replaces_existing() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_upsert_replaces_existing(&store).await;
    }

    #[tokio::test]
    async fn lancedb_delete_by_document() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_delete_by_document(&store).await;
    }

    #[tokio::test]
    async fn lancedb_delete_nonexistent_document() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_delete_nonexistent_document(&store).await;
    }

    #[tokio::test]
    async fn lancedb_get_chunk() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_get_chunk(&store).await;
    }

    #[tokio::test]
    async fn lancedb_get_chunks_for_document() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_get_chunks_for_document(&store).await;
    }

    #[tokio::test]
    async fn lancedb_delete_by_store() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_delete_by_store(&store).await;
    }

    #[tokio::test]
    async fn lancedb_dense_search_round_trip() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_dense_search_round_trip(&store).await;
    }

    #[tokio::test]
    async fn lancedb_dense_search_limit() {
        let (store, _dir) = fresh_conformance_store().await;
        conformance::test_dense_search_limit(&store).await;
    }

    // --- BM25 tests require FTS index (create it before querying) ---

    #[tokio::test]
    async fn lancedb_bm25_search_round_trip() {
        let (store, _dir) = fresh_store().await;

        // Insert records with text containing known keywords
        let records = vec![
            make_record(
                "chunk-1",
                "doc-1",
                "store-1",
                "The quick brown fox jumps over the lazy dog",
                vec![1.0, 0.0, 0.0, 0.0],
            ),
            make_record(
                "chunk-2",
                "doc-1",
                "store-1",
                "A lazy dog slept in the afternoon",
                vec![0.0, 1.0, 0.0, 0.0],
            ),
            make_record(
                "chunk-3",
                "doc-2",
                "store-1",
                "The fox was quick indeed",
                vec![0.0, 0.0, 1.0, 0.0],
            ),
        ];
        store.upsert_chunks(records).await.unwrap();

        // Create FTS index
        store.create_fts_index().await.unwrap();

        let results = store.bm25_search("fox quick", 3, &[]).await.unwrap();
        assert!(!results.is_empty(), "BM25 should return results");
        let ids: Vec<&str> = results.iter().map(|r| r.chunk.id.as_str()).collect();
        assert!(
            ids.contains(&"chunk-1") || ids.contains(&"chunk-3"),
            "should find fox/quick chunks, got: {:?}",
            ids
        );
    }

    #[tokio::test]
    async fn lancedb_bm25_limit() {
        let (store, _dir) = fresh_store().await;

        let records: Vec<ChunkRecord> = (0..5)
            .map(|i| {
                make_record(
                    &format!("chunk-{i}"),
                    "doc-1",
                    "store-1",
                    &format!("search term test document {i}"),
                    vec![0.5, 0.5, 0.0, 0.0],
                )
            })
            .collect();
        store.upsert_chunks(records).await.unwrap();
        store.create_fts_index().await.unwrap();

        let results = store.bm25_search("search term", 2, &[]).await.unwrap();
        assert_eq!(results.len(), 2, "limit should be respected");
    }

    // --- Metadata filter tests ---

    #[tokio::test]
    async fn lancedb_filter_by_document_id() {
        let (store, _dir) = fresh_store().await;

        let records = vec![
            make_record(
                "chunk-1",
                "doc-A",
                "store-1",
                "first doc text",
                vec![1.0, 0.0, 0.0, 0.0],
            ),
            make_record(
                "chunk-2",
                "doc-B",
                "store-1",
                "second doc text",
                vec![0.0, 1.0, 0.0, 0.0],
            ),
        ];
        store.upsert_chunks(records).await.unwrap();

        let filter = vec![MetadataFilter::DocumentId("doc-A".to_string())];
        let results = store
            .dense_search(&[1.0, 0.0, 0.0, 0.0], 10, &filter)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.document_id, "doc-A");
    }

    #[tokio::test]
    async fn lancedb_filter_by_source_id() {
        let (store, _dir) = fresh_store().await;

        let mut r1 = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "source A text",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        r1.source_id = "source-A".to_string();
        let mut r2 = make_record(
            "chunk-2",
            "doc-2",
            "store-1",
            "source B text",
            vec![0.0, 1.0, 0.0, 0.0],
        );
        r2.source_id = "source-B".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::SourceId("source-A".to_string())];
        let results = store
            .dense_search(&[1.0, 0.0, 0.0, 0.0], 10, &filter)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    #[tokio::test]
    async fn lancedb_filter_by_policy_version() {
        let (store, _dir) = fresh_store().await;

        let mut r1 = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "v1 text",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        r1.policy_version = "policy-v1".to_string();
        let mut r2 = make_record(
            "chunk-2",
            "doc-2",
            "store-1",
            "v2 text",
            vec![0.0, 1.0, 0.0, 0.0],
        );
        r2.policy_version = "policy-v2".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::PolicyVersion("policy-v1".to_string())];
        let results = store
            .dense_search(&[1.0, 0.0, 0.0, 0.0], 10, &filter)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    // --- Spec-required metadata filter tests (mime, uri prefix, fetched_at range) ---

    #[tokio::test]
    async fn lancedb_filter_by_mime() {
        let (store, _dir) = fresh_store().await;

        let mut r1 = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "markdown content",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        r1.mime = Some("text/markdown".to_string());
        let mut r2 = make_record(
            "chunk-2",
            "doc-2",
            "store-1",
            "html content",
            vec![0.5, 0.5, 0.0, 0.0],
        );
        r2.mime = Some("text/html".to_string());

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::Mime("text/markdown".to_string())];
        let results = store
            .dense_search(&[1.0, 0.0, 0.0, 0.0], 10, &filter)
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "mime filter should return only markdown chunk"
        );
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    #[tokio::test]
    async fn lancedb_filter_by_uri_prefix() {
        let (store, _dir) = fresh_store().await;

        let mut r1 = make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "notes file",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        r1.uri = "file:///home/user/notes/foo.md".to_string();
        let mut r2 = make_record(
            "chunk-2",
            "doc-2",
            "store-1",
            "docs file",
            vec![0.5, 0.5, 0.0, 0.0],
        );
        r2.uri = "file:///home/user/docs/bar.md".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::UriPrefix(
            "file:///home/user/notes/".to_string(),
        )];
        let results = store
            .dense_search(&[1.0, 0.0, 0.0, 0.0], 10, &filter)
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "uri prefix filter should return only notes chunk"
        );
        assert_eq!(results[0].chunk.id, "chunk-1");
    }

    #[tokio::test]
    async fn lancedb_filter_fetched_after() {
        let (store, _dir) = fresh_store().await;

        let mut r1 = make_record(
            "old-chunk",
            "doc-1",
            "store-1",
            "old content",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        r1.fetched_at = "2026-01-01T00:00:00Z".to_string();
        let mut r2 = make_record(
            "new-chunk",
            "doc-2",
            "store-1",
            "new content",
            vec![0.5, 0.5, 0.0, 0.0],
        );
        r2.fetched_at = "2026-06-10T00:00:00Z".to_string();

        store.upsert_chunks(vec![r1, r2]).await.unwrap();

        let filter = vec![MetadataFilter::FetchedAfter(
            "2026-03-01T00:00:00Z".to_string(),
        )];
        let results = store
            .dense_search(&[1.0, 0.0, 0.0, 0.0], 10, &filter)
            .await
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "fetched_after filter should return only the newer chunk"
        );
        assert_eq!(results[0].chunk.id, "new-chunk");
    }

    // --- Schema and helper tests ---

    #[test]
    fn schema_has_correct_columns() {
        let schema = make_schema(128);
        assert!(schema.field_with_name(COL_ID).is_ok());
        assert!(schema.field_with_name(COL_DOCUMENT_ID).is_ok());
        assert!(schema.field_with_name(COL_EMBEDDING).is_ok());
        assert!(schema.field_with_name(COL_TEXT).is_ok());

        // Embedding should be FixedSizeList<Float32>(128)
        let emb_field = schema.field_with_name(COL_EMBEDDING).unwrap();
        match emb_field.data_type() {
            DataType::FixedSizeList(_, dim) => assert_eq!(*dim, 128),
            _ => panic!("embedding should be FixedSizeList"),
        }
    }

    #[test]
    fn filters_to_predicate_empty() {
        assert!(filters_to_predicate(&[]).is_none());
    }

    #[test]
    fn filters_to_predicate_single() {
        let filters = vec![MetadataFilter::Mime("text/plain".to_string())];
        let pred = filters_to_predicate(&filters).unwrap();
        assert!(pred.contains("mime"));
        assert!(pred.contains("text/plain"));
    }

    #[test]
    fn filters_to_predicate_multiple() {
        let filters = vec![
            MetadataFilter::Mime("text/plain".to_string()),
            MetadataFilter::UriPrefix("file:///".to_string()),
        ];
        let pred = filters_to_predicate(&filters).unwrap();
        assert!(pred.contains(" AND "));
    }

    #[test]
    fn escape_sql_handles_quotes() {
        assert_eq!(escape_sql("it's"), "it''s");
        assert_eq!(escape_sql("normal"), "normal");
    }

    #[tokio::test]
    async fn open_creates_table_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let store = LanceDbStore::open(&path, DIM).await.unwrap();
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 0);
    }

    #[tokio::test]
    async fn open_reuses_existing_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();

        // First open: create table and insert
        {
            let store = LanceDbStore::open(&path, DIM).await.unwrap();
            let records = vec![make_record(
                "chunk-1",
                "doc-1",
                "store-1",
                "hello",
                vec![1.0, 0.0, 0.0, 0.0],
            )];
            store.upsert_chunks(records).await.unwrap();
        }

        // Second open: should see existing data
        {
            let store = LanceDbStore::open(&path, DIM).await.unwrap();
            let stats = store.stats().await.unwrap();
            assert_eq!(stats.chunk_count, 1, "data should persist after reopen");
        }
    }

    #[test]
    fn records_to_batch_round_trip() {
        let records = vec![make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "hello world",
            vec![1.0, 0.0, 0.0, 0.0],
        )];
        let batch = records_to_batch(&records, DIM).unwrap();
        assert_eq!(batch.num_rows(), 1);

        // Check the id column
        let id_col = batch
            .column_by_name(COL_ID)
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(id_col.value(0), "chunk-1");
    }

    // A4: dim mismatch in records_to_batch returns an error.
    #[test]
    fn records_to_batch_dim_mismatch_is_error() {
        let records = vec![make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "hello",
            vec![1.0, 0.0, 0.0, 0.0], // dim=4
        )];
        // Request dim=8 but embedding has dim=4 → error
        let result = records_to_batch(&records, 8);
        assert!(
            result.is_err(),
            "dim mismatch in records_to_batch must return Err, not NULL embedding"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("dimension mismatch"),
            "error should mention dimension mismatch: {msg}"
        );
    }

    // A5: opening an existing table with the wrong dim is rejected.
    #[tokio::test]
    async fn open_rejects_mismatched_dim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();

        // Create store with dim=4
        LanceDbStore::open(&path, 4).await.unwrap();

        // Attempt to reopen with dim=8 → should fail
        let result = LanceDbStore::open(&path, 8).await;
        assert!(
            result.is_err(),
            "opening an existing store with wrong dim must fail"
        );
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("dimension mismatch") || msg.contains("dim="),
            "error should mention dimension mismatch: {msg}"
        );
    }
}
