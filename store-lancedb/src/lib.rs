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
    UInt64Array, UInt8Array,
};
use arrow_schema::{DataType, Field, Schema};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use lance_index::scalar::FullTextSearchQuery;
use lancedb::index::{scalar::BTreeIndexBuilder, vector::IvfFlatIndexBuilder, Index, IndexType};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{connect, DistanceType, Table};

use localdb_core::ingestion::DocumentRecord;
use localdb_core::store::{ChunkRecord, MetadataFilter, RetrievalStore, SearchResult, StoreStats};
use localdb_core::types::Span;
use localdb_core::{Error, VectorEncoding};

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
const COL_DC: &str = "metadata";

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
    encoding: VectorEncoding,
}

impl LanceDbStore {
    /// Open or create a LanceDB store at the given path.
    ///
    /// # Arguments
    /// * `path` - Directory path for this store's LanceDB database.
    /// * `embedding_dim` - Dimension of the embedding vectors. Must be consistent.
    /// * `encoding` - How vectors are stored: `Float32` or `Binary`.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or the table cannot be created,
    /// or if the stored schema's encoding or dimension doesn't match.
    pub async fn open(
        path: &str,
        embedding_dim: usize,
        encoding: VectorEncoding,
    ) -> Result<Self, Error> {
        let db = connect(path)
            .read_consistency_interval(std::time::Duration::ZERO)
            .execute()
            .await
            .map_err(|e| Error::Internal {
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

            // Validate stored encoding and dim match what the embedder expects.
            let schema = table.schema().await.map_err(|e| Error::Internal {
                message: format!("LanceDB schema read failed: {e}"),
                correlation_id: "lancedb-schema".to_string(),
            })?;
            if let Ok(field) = schema.field_with_name(COL_EMBEDDING) {
                if let DataType::FixedSizeList(item_field, col_dim) = field.data_type() {
                    let stored_encoding = match item_field.data_type() {
                        DataType::Float32 => VectorEncoding::Float32,
                        DataType::UInt8 => VectorEncoding::Binary,
                        other => {
                            return Err(Error::Internal {
                                message: format!(
                                    "unexpected embedding item type in store at '{path}': {other:?}"
                                ),
                                correlation_id: "lancedb-schema".to_string(),
                            });
                        }
                    };
                    let stored_orig_dim = if stored_encoding == VectorEncoding::Binary {
                        *col_dim as usize * 8
                    } else {
                        *col_dim as usize
                    };
                    if stored_encoding != encoding {
                        return Err(Error::InvalidConfig {
                            message: format!(
                                "vector encoding mismatch: store at '{path}' uses {stored_encoding:?} \
                                 encoding but the current embedder requires {encoding:?}. \
                                 Delete the store directory and re-run `localdb index` to rebuild it.",
                            ),
                        });
                    }
                    if stored_orig_dim != embedding_dim {
                        return Err(Error::InvalidConfig {
                            message: format!(
                                "embedding dimension mismatch: store at '{path}' was created with \
                                 dim={stored_orig_dim} but the current embedder produces \
                                 dim={embedding_dim}. Update your embedding config to match the \
                                 stored dimension, or delete the store and re-run `localdb index` \
                                 to rebuild it.",
                            ),
                        });
                    }
                }
            }
            table
        } else {
            // Create the table with the schema
            let schema = make_schema(embedding_dim, encoding);
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
            encoding,
        })
    }

    /// Return true if a full-text (INVERTED) index exists on the `text` column.
    ///
    /// Cheap metadata lookup (`list_indices`). Used both to repair a missing FTS
    /// index on re-index and to let `bm25_search` degrade gracefully when absent.
    pub async fn has_fts_index(&self) -> Result<bool, Error> {
        let indices = self
            .table
            .list_indices()
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB list_indices failed: {e}"),
                correlation_id: "lancedb-list-indices".to_string(),
            })?;
        // Match the FTS type explicitly, not just the column: a future scalar
        // index on `text` must not be mistaken for the full-text index (that would
        // resurface the "INVERTED index required" failure this guard prevents).
        Ok(indices.iter().any(|cfg| {
            cfg.index_type == IndexType::FTS && cfg.columns.iter().any(|c| c == COL_TEXT)
        }))
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

    /// Create an IVF_FLAT/Hamming vector index on the binary embedding column.
    ///
    /// No-op for Float32 stores (flat KNN is used instead).
    /// Skipped when the table has fewer than 256 rows (flat Hamming scan is correct
    /// and IVF training needs a minimum sample set).
    /// Safe to call multiple times (LanceDB replaces the existing index).
    pub async fn create_vector_index(&self) -> Result<(), Error> {
        if self.encoding != VectorEncoding::Binary {
            return Ok(());
        }

        let row_count = self
            .table
            .count_rows(None)
            .await
            .map_err(|e| Error::Internal {
                message: format!("row count for vector index check failed: {e}"),
                correlation_id: "lancedb-vector-index-count".to_string(),
            })?;

        // Need enough rows for meaningful IVF centroid training.
        if row_count < 256 {
            return Ok(());
        }

        self.table
            .create_index(
                &[COL_EMBEDDING],
                Index::IvfFlat(IvfFlatIndexBuilder::default().distance_type(DistanceType::Hamming)),
            )
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("IVF_FLAT/Hamming index creation failed: {e}"),
                correlation_id: "lancedb-vector-index".to_string(),
            })
    }
}

// ---------------------------------------------------------------------------
// Schema and batch construction helpers
// ---------------------------------------------------------------------------

/// Build the Arrow schema for the chunks table.
fn make_schema(embedding_dim: usize, encoding: VectorEncoding) -> Arc<Schema> {
    let embedding_field = match encoding {
        VectorEncoding::Float32 => Field::new(
            COL_EMBEDDING,
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                embedding_dim as i32,
            ),
            false,
        ),
        VectorEncoding::Binary => Field::new(
            COL_EMBEDDING,
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::UInt8, true)),
                (embedding_dim / 8) as i32,
            ),
            false,
        ),
    };
    Arc::new(Schema::new(vec![
        Field::new(COL_ID, DataType::Utf8, false),
        Field::new(COL_DOCUMENT_ID, DataType::Utf8, false),
        Field::new(COL_STORE_ID, DataType::Utf8, false),
        Field::new(COL_TEXT, DataType::Utf8, false),
        Field::new(COL_SPAN_START, DataType::UInt64, false),
        Field::new(COL_SPAN_END, DataType::UInt64, false),
        Field::new(COL_HEADING_PATH, DataType::Utf8, false), // JSON-encoded
        embedding_field,
        Field::new(COL_POLICY_VERSION, DataType::Utf8, false),
        Field::new(COL_FETCHED_AT, DataType::Utf8, false),
        Field::new(COL_CONTENT_HASH, DataType::Utf8, false),
        Field::new(COL_ORIGIN_STORE, DataType::Utf8, false),
        Field::new(COL_SOURCE_ID, DataType::Utf8, false),
        Field::new(COL_SOURCE_KIND, DataType::Utf8, false),
        Field::new(COL_MIME, DataType::Utf8, true), // nullable
        Field::new(COL_URI, DataType::Utf8, false),
        Field::new(COL_TITLE, DataType::Utf8, true), // nullable
        Field::new(COL_DC, DataType::Utf8, true),    // nullable; JSON-encoded DocumentMetadata
    ]))
}

/// Build an empty RecordBatch for table initialization.
fn make_empty_batch(schema: &Arc<Schema>) -> RecordBatch {
    let emb_field = schema.field_with_name(COL_EMBEDDING).unwrap();
    let (col_dim, is_binary) = match emb_field.data_type() {
        DataType::FixedSizeList(item, dim) => (*dim, matches!(item.data_type(), DataType::UInt8)),
        _ => panic!("unexpected embedding type"),
    };
    let embedding_col: Arc<dyn Array> = if is_binary {
        Arc::new(FixedSizeListArray::from_iter_primitive::<
            arrow_array::types::UInt8Type,
            _,
            _,
        >(
            std::iter::empty::<Option<Vec<Option<u8>>>>(), col_dim
        ))
    } else {
        Arc::new(FixedSizeListArray::from_iter_primitive::<
            arrow_array::types::Float32Type,
            _,
            _,
        >(
            std::iter::empty::<Option<Vec<Option<f32>>>>(), col_dim
        ))
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
            embedding_col,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<String>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<&str>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<String>>::new())) as _,
            Arc::new(StringArray::from(Vec::<Option<String>>::new())) as _, // metadata
        ],
    )
    .expect("empty batch construction failed")
}

/// Pack an f32 slice to binary bytes, MSB-first.
///
/// Each float is thresholded at zero: `x ≥ 0.0` → bit 1, `x < 0.0` → bit 0.
/// Packing order: dim 0 → bit 7 of byte 0, dim 1 → bit 6 of byte 0, ..., dim 7 → bit 0.
/// This matches `np.packbits(x >= 0, axis=-1)` (NumPy MSB-first, the paper standard).
fn binarize_msb(v: &[f32]) -> Vec<u8> {
    v.chunks(8)
        .map(|chunk| {
            let mut byte = 0u8;
            for (i, &val) in chunk.iter().enumerate() {
                if val >= 0.0 {
                    byte |= 1 << (7 - i);
                }
            }
            byte
        })
        .collect()
}

/// Build a RecordBatch from a slice of ChunkRecords.
///
/// All records must have the same embedding dimension, and it must match `embedding_dim`.
fn records_to_batch(
    records: &[ChunkRecord],
    embedding_dim: usize,
    encoding: VectorEncoding,
) -> Result<RecordBatch, Error> {
    let schema = make_schema(embedding_dim, encoding);

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
    let metadata_jsons: Vec<Option<String>> = records
        .iter()
        .map(|r| serde_json::to_string(&r.metadata).ok())
        .collect();

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
    let embeddings: Arc<dyn Array> = match encoding {
        VectorEncoding::Float32 => Arc::new(FixedSizeListArray::from_iter_primitive::<
            arrow_array::types::Float32Type,
            _,
            _,
        >(
            records
                .iter()
                .map(|r| Some(r.embedding.iter().map(|&v| Some(v)).collect::<Vec<_>>())),
            embedding_dim as i32,
        )),
        VectorEncoding::Binary => {
            let binary_dim = (embedding_dim / 8) as i32;
            Arc::new(FixedSizeListArray::from_iter_primitive::<
                arrow_array::types::UInt8Type,
                _,
                _,
            >(
                records.iter().map(|r| {
                    let bytes = binarize_msb(&r.embedding);
                    Some(bytes.into_iter().map(Some).collect::<Vec<_>>())
                }),
                binary_dim,
            ))
        }
    };

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
            embeddings,
            Arc::new(StringArray::from(policy_versions)) as _,
            Arc::new(StringArray::from(fetched_ats)) as _,
            Arc::new(StringArray::from(content_hashes)) as _,
            Arc::new(StringArray::from(origin_stores)) as _,
            Arc::new(StringArray::from(source_ids)) as _,
            Arc::new(StringArray::from(source_kinds)) as _,
            Arc::new(StringArray::from(mimes)) as _,
            Arc::new(StringArray::from(uris)) as _,
            Arc::new(StringArray::from(titles)) as _,
            Arc::new(StringArray::from(metadata_jsons)) as _,
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

    // Read metadata defensively: pre-migration tables lack the column → default.
    let metadata = get_opt_str(batch, COL_DC, row)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

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
        metadata,
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
        let batch = records_to_batch(&records, self.embedding_dim, self.encoding)?;
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
        match self.encoding {
            VectorEncoding::Float32 => {
                self.float32_dense_search(query_vector, limit, filters)
                    .await
            }
            VectorEncoding::Binary => self.binary_dense_search(query_vector, limit, filters).await,
        }
    }

    async fn bm25_search(
        &self,
        query_text: &str,
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        // Degrade gracefully when no INVERTED index exists yet (e.g. a cancelled
        // index run committed chunks but never reached the FTS build). LanceDB has
        // no flat fallback for full-text search, so without this the whole hybrid
        // query would abort; instead the BM25 leg contributes nothing and dense
        // search carries the result.
        if !self.has_fts_index().await? {
            return Ok(Vec::new());
        }

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

    async fn list_indexed_documents(&self) -> Result<Vec<DocumentRecord>, Error> {
        let chunk_count = self
            .table
            .count_rows(None)
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB count_rows failed: {e}"),
                correlation_id: "list-docs-count".to_string(),
            })?;

        if chunk_count == 0 {
            return Ok(vec![]);
        }

        let stream = self
            .table
            .query()
            .select(lancedb::query::Select::columns(&[
                COL_URI,
                COL_DOCUMENT_ID,
                COL_CONTENT_HASH,
                COL_POLICY_VERSION,
            ]))
            .execute()
            .await
            .map_err(|e| Error::Internal {
                message: format!("LanceDB list_indexed_documents query failed: {e}"),
                correlation_id: "list-docs-query".to_string(),
            })?;

        let batches: Vec<RecordBatch> =
            stream.try_collect().await.map_err(|e| Error::Internal {
                message: format!("LanceDB list_indexed_documents stream failed: {e}"),
                correlation_id: "list-docs-stream".to_string(),
            })?;

        let mut seen: std::collections::HashMap<String, DocumentRecord> =
            std::collections::HashMap::new();
        for batch in &batches {
            let uri_col = batch
                .column_by_name(COL_URI)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let doc_id_col = batch
                .column_by_name(COL_DOCUMENT_ID)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let hash_col = batch
                .column_by_name(COL_CONTENT_HASH)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let policy_col = batch
                .column_by_name(COL_POLICY_VERSION)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            if let (Some(uris), Some(doc_ids), Some(hashes), Some(policies)) =
                (uri_col, doc_id_col, hash_col, policy_col)
            {
                for row in 0..batch.num_rows() {
                    if uris.is_null(row) {
                        continue;
                    }
                    let uri = uris.value(row).to_string();
                    seen.entry(uri.clone()).or_insert(DocumentRecord {
                        uri,
                        document_id: if doc_ids.is_null(row) {
                            String::new()
                        } else {
                            doc_ids.value(row).to_string()
                        },
                        content_hash: if hashes.is_null(row) {
                            String::new()
                        } else {
                            hashes.value(row).to_string()
                        },
                        policy_version: if policies.is_null(row) {
                            String::new()
                        } else {
                            policies.value(row).to_string()
                        },
                    });
                }
            }
        }

        Ok(seen.into_values().collect())
    }
}

// ---------------------------------------------------------------------------
// Dense search helpers
// ---------------------------------------------------------------------------

impl LanceDbStore {
    /// Dense search using the standard `nearest_to` path (Float32 stores).
    async fn float32_dense_search(
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
                correlation_id: "dense-nearest".to_string(),
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

    /// Dense search via the lance `dataset()` bypass for binary (UInt8) stores.
    ///
    /// `nearest_to` hard-codes Float32 (`lancedb/src/query.rs`), so we go through
    /// `Table::dataset()` → `Dataset::scan()` → `Scanner::nearest()`, which accepts
    /// any `Array` type and auto-selects Hamming for UInt8.
    ///
    /// Score formula: `1.0 - hamming_dist / nbits` maps raw Hamming distance (0..nbits)
    /// into [0, 1] with 1.0 = identical vectors.
    async fn binary_dense_search(
        &self,
        query_vector: &[f32],
        limit: usize,
        filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>, Error> {
        let query_bytes = binarize_msb(query_vector);
        let nbits = query_vector.len();
        let query_array = UInt8Array::from(query_bytes);

        let dataset_wrapper = self.table.dataset().ok_or_else(|| Error::Internal {
            message: "binary_dense_search: Table::dataset() returned None (remote table?)".into(),
            correlation_id: "binary-dataset".to_string(),
        })?;

        let guard = dataset_wrapper.get().await.map_err(|e| Error::Internal {
            message: format!("binary_dense_search: dataset get failed: {e}"),
            correlation_id: "binary-dataset-get".to_string(),
        })?;

        // Scanner clones Arc<Dataset> internally, so it outlives the guard.
        let mut scanner = guard.scan();

        if let Some(predicate) = filters_to_predicate(filters) {
            scanner.filter(&predicate).map_err(|e| Error::Internal {
                message: format!("binary_dense_search: scanner filter failed: {e}"),
                correlation_id: "binary-filter".to_string(),
            })?;
        }

        scanner
            .nearest(COL_EMBEDDING, &query_array, limit)
            .map_err(|e| Error::Internal {
                message: format!("binary_dense_search: scanner nearest failed: {e}"),
                correlation_id: "binary-nearest".to_string(),
            })?;

        let stream = scanner
            .try_into_stream()
            .await
            .map_err(|e| Error::Internal {
                message: format!("binary_dense_search: try_into_stream failed: {e}"),
                correlation_id: "binary-stream".to_string(),
            })?;

        futures::pin_mut!(stream);
        let mut results = Vec::new();
        while let Some(item) = stream.next().await {
            let batch = item.map_err(|e| Error::Internal {
                message: format!("binary_dense_search: stream item failed: {e}"),
                correlation_id: "binary-stream-item".to_string(),
            })?;
            for row in 0..batch.num_rows() {
                let record = row_to_chunk_record(&batch, row);
                let hamming_dist = get_f32(&batch, COL_DISTANCE, row).unwrap_or(0.0);
                let score = 1.0 - hamming_dist / nbits as f32;
                results.push(SearchResult {
                    chunk: record,
                    score,
                });
            }
        }
        Ok(results)
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
            metadata: localdb_core::parser::DocumentMetadata::default(),
        }
    }

    /// Create a fresh store with DIM=4 (local tests).
    async fn fresh_store() -> (LanceDbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let store = LanceDbStore::open(&path, DIM, VectorEncoding::Float32)
            .await
            .unwrap();
        (store, dir)
    }

    /// Create a fresh store with DIM=2 (conformance suite uses 2D vectors).
    async fn fresh_conformance_store() -> (LanceDbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let store = LanceDbStore::open(&path, CONFORMANCE_DIM, VectorEncoding::Float32)
            .await
            .unwrap();
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
    async fn lancedb_metadata_round_trip() {
        let (store, _dir) = fresh_store().await;

        let dc = localdb_core::parser::DocumentMetadata {
            title: Some("Test Document".to_string()),
            creator: vec!["Alice".to_string(), "Bob".to_string()],
            date: Some("2026-06-13".to_string()),
            language: Some("en".to_string()),
            ..Default::default()
        };

        let mut record = make_record(
            "dc-chunk-1",
            "doc-dc-1",
            "store-1",
            "Dublin Core test text",
            vec![0.1, 0.2, 0.3, 0.4],
        );
        record.metadata = dc.clone();

        store.upsert_chunks(vec![record]).await.unwrap();
        store.create_fts_index().await.unwrap();

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 1);

        let results = store
            .bm25_search("Dublin Core test", 10, &[])
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].chunk.metadata, dc,
            "Dublin Core must round-trip through LanceDB"
        );
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

    #[tokio::test]
    async fn lancedb_has_fts_index_reflects_creation() {
        let (store, _dir) = fresh_store().await;
        let records = vec![make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "indexable text",
            vec![1.0, 0.0, 0.0, 0.0],
        )];
        store.upsert_chunks(records).await.unwrap();

        assert!(
            !store.has_fts_index().await.unwrap(),
            "no FTS index before creation"
        );

        // A non-FTS index (BTree on document_id) must not be mistaken for the FTS
        // index — has_fts_index discriminates on index type, not just column.
        store.create_document_id_index().await.unwrap();
        assert!(
            !store.has_fts_index().await.unwrap(),
            "BTree index must not count as an FTS index"
        );

        store.create_fts_index().await.unwrap();
        assert!(
            store.has_fts_index().await.unwrap(),
            "FTS index present after creation"
        );
    }

    #[tokio::test]
    async fn lancedb_bm25_degrades_without_index() {
        // Reproduces the cancelled-run state: chunks committed, no FTS index built.
        // bm25_search must degrade to an empty result rather than erroring.
        let (store, _dir) = fresh_store().await;
        let records = vec![make_record(
            "chunk-1",
            "doc-1",
            "store-1",
            "The quick brown fox",
            vec![1.0, 0.0, 0.0, 0.0],
        )];
        store.upsert_chunks(records).await.unwrap();

        let results = store
            .bm25_search("fox", 10, &[])
            .await
            .expect("bm25_search must not error when FTS index is missing");
        assert!(
            results.is_empty(),
            "BM25 leg degrades to empty without an FTS index"
        );

        // And once the index exists, BM25 returns results again.
        store.create_fts_index().await.unwrap();
        let results = store.bm25_search("fox", 10, &[]).await.unwrap();
        assert!(!results.is_empty(), "BM25 works after the index is built");
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
        let schema = make_schema(128, VectorEncoding::Float32);
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
        let store = LanceDbStore::open(&path, DIM, VectorEncoding::Float32)
            .await
            .unwrap();
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 0);
    }

    #[tokio::test]
    async fn open_reuses_existing_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();

        // First open: create table and insert
        {
            let store = LanceDbStore::open(&path, DIM, VectorEncoding::Float32)
                .await
                .unwrap();
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
            let store = LanceDbStore::open(&path, DIM, VectorEncoding::Float32)
                .await
                .unwrap();
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
        let batch = records_to_batch(&records, DIM, VectorEncoding::Float32).unwrap();
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
        let result = records_to_batch(&records, 8, VectorEncoding::Float32);
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
        LanceDbStore::open(&path, 4, VectorEncoding::Float32)
            .await
            .unwrap();

        // Attempt to reopen with dim=8 → should fail
        let result = LanceDbStore::open(&path, 8, VectorEncoding::Float32).await;
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

    // -----------------------------------------------------------------------
    // Binary encoding tests
    // -----------------------------------------------------------------------

    #[test]
    fn binarize_msb_known_vectors() {
        // All positive → all ones → 0xFF per byte
        let all_pos = vec![1.0f32; 8];
        assert_eq!(binarize_msb(&all_pos), vec![0xFF]);

        // All negative → all zeros → 0x00 per byte
        let all_neg = vec![-1.0f32; 8];
        assert_eq!(binarize_msb(&all_neg), vec![0x00]);

        // First positive, rest negative → bit 7 set → 0x80
        let first_pos = vec![1.0, -1.0, -1.0, -1.0, -1.0, -1.0, -1.0, -1.0];
        assert_eq!(binarize_msb(&first_pos), vec![0x80]);

        // Last positive, rest negative → bit 0 set → 0x01
        let last_pos = vec![-1.0, -1.0, -1.0, -1.0, -1.0, -1.0, -1.0, 1.0];
        assert_eq!(binarize_msb(&last_pos), vec![0x01]);

        // Zero is treated as positive (x >= 0.0)
        let zero = vec![0.0f32; 8];
        assert_eq!(binarize_msb(&zero), vec![0xFF]);

        // 16-dim: two bytes
        let v16: Vec<f32> = (0..16)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        // even dims positive: bits 7,5,3,1 set in each byte → 0b10101010 = 0xAA
        assert_eq!(binarize_msb(&v16), vec![0xAA, 0xAA]);
    }

    #[test]
    fn binarize_msb_output_length() {
        assert_eq!(binarize_msb(&[1.0f32; 8]).len(), 1);
        assert_eq!(binarize_msb(&[1.0f32; 16]).len(), 2);
        assert_eq!(binarize_msb(&[1.0f32; 1024]).len(), 128);
    }

    #[tokio::test]
    async fn binary_store_write_and_search() {
        // DIM=8: binary column will be 1 byte per vector.
        const BDIM: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let store = LanceDbStore::open(&path, BDIM, VectorEncoding::Binary)
            .await
            .unwrap();

        // Two records: one all-positive embedding, one all-negative.
        let rec_pos = make_record(
            "bin-pos",
            "doc-bin",
            "store-bin",
            "all positive",
            vec![1.0f32; BDIM],
        );
        let rec_neg = make_record(
            "bin-neg",
            "doc-bin",
            "store-bin",
            "all negative",
            vec![-1.0f32; BDIM],
        );
        store.upsert_chunks(vec![rec_pos, rec_neg]).await.unwrap();
        store.create_fts_index().await.unwrap();
        // No create_vector_index: only 2 rows, flat fallback is correct.

        let stats = store.stats().await.unwrap();
        assert_eq!(stats.chunk_count, 2);

        // Query with all-positive vector — should rank "bin-pos" first.
        let query = vec![1.0f32; BDIM];
        let results = store.dense_search(&query, 2, &[]).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].chunk.id, "bin-pos",
            "all-positive query should rank all-positive doc first"
        );
        assert!(
            results[0].score > results[1].score,
            "first result should have higher score"
        );
    }

    #[tokio::test]
    async fn binary_store_small_flat_fallback() {
        // With fewer than 256 rows, create_vector_index is a no-op (flat scan used).
        const BDIM: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();
        let store = LanceDbStore::open(&path, BDIM, VectorEncoding::Binary)
            .await
            .unwrap();

        let rec = make_record("b1", "d1", "s1", "hello", vec![1.0f32; BDIM]);
        store.upsert_chunks(vec![rec]).await.unwrap();

        // Should complete without error even though row count < 256.
        let result = store.create_vector_index().await;
        assert!(
            result.is_ok(),
            "create_vector_index should be a no-op on small stores: {result:?}"
        );
    }

    #[tokio::test]
    async fn encoding_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();

        // Create store as Float32
        LanceDbStore::open(&path, 8, VectorEncoding::Float32)
            .await
            .unwrap();

        // Attempt to reopen as Binary → must fail
        let result = LanceDbStore::open(&path, 8, VectorEncoding::Binary).await;
        assert!(result.is_err(), "encoding mismatch must be rejected");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("encoding mismatch") || msg.contains("Float32") || msg.contains("Binary"),
            "error should describe the encoding mismatch: {msg}"
        );
    }

    // -------------------------------------------------------------------------
    // list_indexed_documents tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn list_indexed_documents_empty_store() {
        let (store, _dir) = fresh_store().await;
        let records = store.list_indexed_documents().await.unwrap();
        assert!(records.is_empty(), "empty store must return empty vec");
    }

    #[tokio::test]
    async fn list_indexed_documents_returns_one_record_per_document() {
        let (store, _dir) = fresh_store().await;

        // Two documents, two chunks each.
        let doc1_chunk1 = make_record(
            "c1",
            "doc-1",
            "store-1",
            "first chunk of doc1",
            vec![1.0, 0.0, 0.0, 0.0],
        );
        let mut doc1_chunk2 = make_record(
            "c2",
            "doc-1",
            "store-1",
            "second chunk of doc1",
            vec![0.9, 0.1, 0.0, 0.0],
        );
        doc1_chunk2.uri = doc1_chunk1.uri.clone();
        doc1_chunk2.content_hash = doc1_chunk1.content_hash.clone();
        doc1_chunk2.policy_version = doc1_chunk1.policy_version.clone();

        let mut doc2_chunk1 = make_record(
            "c3",
            "doc-2",
            "store-1",
            "first chunk of doc2",
            vec![0.0, 1.0, 0.0, 0.0],
        );
        doc2_chunk1.uri = "file:///doc2.md".to_string();
        doc2_chunk1.content_hash = "hash-doc2".to_string();

        let mut doc2_chunk2 = make_record(
            "c4",
            "doc-2",
            "store-1",
            "second chunk of doc2",
            vec![0.0, 0.9, 0.1, 0.0],
        );
        doc2_chunk2.uri = doc2_chunk1.uri.clone();
        doc2_chunk2.content_hash = doc2_chunk1.content_hash.clone();
        doc2_chunk2.policy_version = doc2_chunk1.policy_version.clone();
        doc2_chunk2.document_id = doc2_chunk1.document_id.clone();

        store
            .upsert_chunks(vec![
                doc1_chunk1.clone(),
                doc1_chunk2,
                doc2_chunk1,
                doc2_chunk2,
            ])
            .await
            .unwrap();

        let records = store.list_indexed_documents().await.unwrap();
        assert_eq!(records.len(), 2, "two documents → two records");

        // Look up by URI for deterministic assertions regardless of order.
        let rec1 = records
            .iter()
            .find(|r| r.uri == doc1_chunk1.uri)
            .expect("doc1 record missing");
        let rec2 = records
            .iter()
            .find(|r| r.uri == "file:///doc2.md")
            .expect("doc2 record missing");

        assert_eq!(rec1.document_id, "doc-1");
        assert_eq!(rec1.content_hash, doc1_chunk1.content_hash);
        assert_eq!(rec1.policy_version, "v1");
        assert_eq!(rec2.document_id, "doc-2");
        assert_eq!(rec2.content_hash, "hash-doc2");
    }

    #[tokio::test]
    async fn list_indexed_documents_correct_hashes_and_policy() {
        let (store, _dir) = fresh_store().await;

        let mut record = make_record(
            "chunk-1",
            "doc-abc",
            "store-1",
            "some text",
            vec![0.5, 0.5, 0.0, 0.0],
        );
        record.uri = "file:///unique.md".to_string();
        record.content_hash = "deadbeef123".to_string();
        record.policy_version = "policy-v2".to_string();

        store.upsert_chunks(vec![record]).await.unwrap();

        let records = store.list_indexed_documents().await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].uri, "file:///unique.md");
        assert_eq!(records[0].document_id, "doc-abc");
        assert_eq!(records[0].content_hash, "deadbeef123");
        assert_eq!(records[0].policy_version, "policy-v2");
    }

    // -----------------------------------------------------------------------
    // Cross-handle consistency (MCP scenario)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cross_handle_sees_fts_index_and_data() {
        // Simulates the MCP server scenario: handle A is opened first (long-lived),
        // then handle B inserts data and creates the FTS index. Handle A must see
        // the new index and be able to BM25 search — this fails without
        // `read_consistency_interval(Duration::ZERO)`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap().to_string();

        let handle_a = LanceDbStore::open(&path, DIM, VectorEncoding::Float32)
            .await
            .unwrap();

        let handle_b = LanceDbStore::open(&path, DIM, VectorEncoding::Float32)
            .await
            .unwrap();

        let records = vec![
            make_record(
                "cross-1",
                "doc-cross",
                "store-cross",
                "The quick brown fox jumps over the lazy dog",
                vec![1.0, 0.0, 0.0, 0.0],
            ),
            make_record(
                "cross-2",
                "doc-cross",
                "store-cross",
                "A lazy dog slept in the afternoon sun",
                vec![0.0, 1.0, 0.0, 0.0],
            ),
        ];
        handle_b.upsert_chunks(records).await.unwrap();
        handle_b.create_fts_index().await.unwrap();

        assert!(
            handle_a.has_fts_index().await.unwrap(),
            "stale handle must see the FTS index created by another handle"
        );

        let results = handle_a.bm25_search("fox", 10, &[]).await.unwrap();
        assert!(
            !results.is_empty(),
            "stale handle must return BM25 results for data written by another handle"
        );
        assert_eq!(results[0].chunk.id, "cross-1");
    }
}
