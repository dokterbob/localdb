use libsql::params;
use localdb_core::ingestion::DocumentRecord;
use localdb_core::{
    compute_metadata_hash, content_hash, ChunkRecord, Error, Metadata, MetadataFilter,
    SearchResult, StoreStats, VectorEncoding,
};

use super::rows::{row_to_block, row_to_chunk_record_strict};
use super::sql::{build_filter_clauses, escape_fts5_query};
use super::TenantStore;
use crate::connection::map_libsql_err;
use crate::vectors;

// Column projection shared across all chunk queries.
//
// The `resources` table replaces the old `documents` table. Field name mapping:
//   resources.added_at      → ChunkRecord.fetched_at
//   resources.modified_at   → ChunkRecord.modified_at
//   resources.metadata_json → ChunkRecord.metadata
//
// Column indices in the SELECT list (used in rows.rs):
//   0  c.id
//   1  c.resource_id
//   2  c.text
//   3  c.heading_path
//   4  vector_extract(c.embedding) AS embedding_json
//   5  r.store_id
//   6  r.source_id
//   7  r.ingestor_kind
//   8  r.uri
//   9  r.title
//  10  r.mime
//  11  r.policy_version
//  12  r.added_at         (→ fetched_at)
//  13  r.modified_at      (→ modified_at)
//  14  r.content_hash
//  15  r.origin_store
//  16  r.metadata_json    (→ metadata)
//  17  c.block_seq
//  18  c.seq_in_block
//  19  c.location_json
//  20  c.block_kind
//  21  distance/score     (appended by each query)
const CHUNK_COLS: &str = "c.id, c.resource_id,
                    c.text, c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    r.store_id, r.source_id, r.ingestor_kind, r.uri, r.title, r.mime,
                    r.policy_version, r.added_at, r.modified_at, r.content_hash, r.origin_store,
                    r.metadata_json, c.block_seq, c.seq_in_block, c.location_json, c.block_kind";

pub(crate) async fn dense_search(
    store: &TenantStore,
    query_vector: &[f32],
    limit: usize,
    filters: &[MetadataFilter],
) -> Result<Vec<SearchResult>, Error> {
    let conn = store.conn().reader();
    let (filter_clauses, filter_values) = build_filter_clauses(filters);
    let encoding = store.encoding();
    let dim = store.embedding_dim();
    // Always start with an overfetch multiplier: the tenant predicate
    // (WHERE c.store_id = ?) acts as a post-ANN filter even when the
    // caller supplies no MetadataFilters.
    let mut fetch_k = limit * 3;
    let max_fetch = limit * 20;

    let mut results = Vec::new();
    let mut ann_saturated = false;
    loop {
        let qvec_sql = vectors::query_vector_sql(query_vector, encoding);
        // TODO(#104): libsql has no partial vector indexes or ANN-level
        // predicate pushdown, so we always overfetch at the global index and
        // post-filter by store_id.  True per-store ANN partitioning would
        // require per-store chunk tables — see the tracking issue.
        // An exact-scan fallback below handles saturation by other tenants.
        //
        // `qvec_sql`, `fetch_k`, and `limit` stay interpolated (issue #255):
        // `qvec_sql` is a `vector32(...)`/`vector1bit(...)` SQL function-call
        // literal built from internally-computed `f32` values — it is not
        // attacker-reachable and not bindable in the `vector_top_k` argument
        // position; `fetch_k`/`limit` are Rust-computed `usize`.
        let sql = format!(
            "SELECT {CHUNK_COLS},
                    vector_distance_cos(c.embedding, {qvec_sql}) AS distance
             FROM vector_top_k('chunks_vec_idx', {qvec_sql}, {fetch_k}) AS v
             JOIN chunks c ON c.rowid = v.id
             JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
             WHERE c.store_id = ?
             {filter_clauses}
             ORDER BY distance ASC
             LIMIT {limit}"
        );
        // `filter_values` is reused across this fetch_k-doubling loop and the
        // exact-scan fallback below, so it is cloned at each query call —
        // `IntoParams::into_params` takes `self` by value.
        let mut params = vec![store.store_id().to_string()];
        params.extend(filter_values.clone());
        let mut rows = conn.query(&sql, params).await.map_err(map_libsql_err)?;
        results.clear();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            let chunk = row_to_chunk_record_strict(&row)?;
            let distance: f64 = row.get(21).map_err(map_libsql_err)?;
            let score = match encoding {
                VectorEncoding::Float32 => vectors::cosine_distance_to_score(distance),
                VectorEncoding::Binary => vectors::hamming_distance_to_score(distance, dim),
            };
            results.push(SearchResult { chunk, score });
        }
        if results.len() >= limit {
            break;
        }
        if fetch_k >= max_fetch {
            ann_saturated = true;
            break;
        }
        fetch_k = (fetch_k * 2).min(max_fetch);
    }

    // Exact-scan fallback: only runs when ANN was truly saturated by other
    // tenants (loop hit max_fetch without filling the tenant's quota). Skips
    // stores that simply have fewer than `limit` chunks — those already got
    // all their results from the ANN pass. Per-store ANN partitioning is the
    // long-term fix (tracking issue).
    if ann_saturated && results.len() < limit {
        let qvec_sql = vectors::query_vector_sql(query_vector, encoding);
        let sql = format!(
            "SELECT {CHUNK_COLS},
                    vector_distance_cos(c.embedding, {qvec_sql}) AS distance
             FROM chunks c
             JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
             WHERE c.store_id = ?
             {filter_clauses}
             ORDER BY distance ASC
             LIMIT {limit}"
        );
        let mut params = vec![store.store_id().to_string()];
        params.extend(filter_values.clone());
        let mut rows = conn.query(&sql, params).await.map_err(map_libsql_err)?;
        results.clear();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            let chunk = row_to_chunk_record_strict(&row)?;
            let distance: f64 = row.get(21).map_err(map_libsql_err)?;
            let score = match encoding {
                VectorEncoding::Float32 => vectors::cosine_distance_to_score(distance),
                VectorEncoding::Binary => vectors::hamming_distance_to_score(distance, dim),
            };
            results.push(SearchResult { chunk, score });
        }
    }
    Ok(results)
}

pub(crate) async fn bm25_search(
    store: &TenantStore,
    query_text: &str,
    limit: usize,
    filters: &[MetadataFilter],
) -> Result<Vec<SearchResult>, Error> {
    if query_text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let conn = store.conn().reader();
    let escaped_query = escape_fts5_query(query_text);
    let (filter_clauses, filter_values) = build_filter_clauses(filters);
    let sql = format!(
        "SELECT {CHUNK_COLS},
                bm25(chunks_fts) AS score
         FROM chunks_fts f
         JOIN chunks c ON c.rowid = f.rowid
         JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
         WHERE chunks_fts MATCH ?
         AND c.store_id = ?
         {filter_clauses}
         ORDER BY score ASC
         LIMIT {limit}"
    );
    // `MATCH ?` is bound first, so it must stay the first positional param;
    // store_id and filter values follow in the order their `?` placeholders
    // appear in the SQL text above (issue #255).
    let mut params = vec![escaped_query, store.store_id().to_string()];
    params.extend(filter_values);
    let mut rows = conn.query(&sql, params).await.map_err(map_libsql_err)?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        let chunk = row_to_chunk_record_strict(&row)?;
        let raw_score: f64 = row.get(21).map_err(map_libsql_err)?;
        results.push(SearchResult {
            chunk,
            score: -raw_score as f32,
        });
    }
    Ok(results)
}

pub(crate) async fn stats(store: &TenantStore) -> Result<StoreStats, Error> {
    let conn = store.conn().reader();
    // A single statement, not two separate `SELECT COUNT(*)` queries: two
    // statements read at two different points in time, so a write could
    // commit between them (in-process now that writes are serialized through
    // one writer connection, and always possible cross-process) and leave
    // chunk_count/document_count mutually inconsistent. Both subqueries here
    // run as one atomic read against a single consistent snapshot.
    let mut rows = conn
        .query(
            "SELECT (SELECT COUNT(*) FROM chunks WHERE store_id = ?) AS chunk_count,
                    (SELECT COUNT(*) FROM resources WHERE store_id = ?) AS document_count",
            params![store.store_id().to_string(), store.store_id().to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let (chunk_count, document_count) = match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => (
            row.get::<u64>(0).map_err(map_libsql_err)?,
            row.get::<u64>(1).map_err(map_libsql_err)?,
        ),
        None => (0, 0),
    };
    Ok(StoreStats {
        chunk_count,
        document_count,
    })
}

pub(crate) async fn get_chunk(
    store: &TenantStore,
    chunk_id: &str,
) -> Result<Option<ChunkRecord>, Error> {
    let conn = store.conn().reader();
    let mut rows = conn
        .query(
            &format!(
                "SELECT {CHUNK_COLS}
                 FROM chunks c
                 JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
                 WHERE c.store_id = ? AND c.id = ?"
            ),
            params![store.store_id().to_string(), chunk_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => Ok(Some(row_to_chunk_record_strict(&row)?)),
        None => Ok(None),
    }
}

pub(crate) async fn get_chunks_for_resource(
    store: &TenantStore,
    resource_id: &str,
) -> Result<Vec<ChunkRecord>, Error> {
    let conn = store.conn().reader();
    let mut rows = conn
        .query(
            &format!(
                "SELECT {CHUNK_COLS}
                 FROM chunks c
                 JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
                 WHERE c.store_id = ? AND c.resource_id = ?
                 ORDER BY c.block_seq, c.seq_in_block"
            ),
            params![store.store_id().to_string(), resource_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_chunk_record_strict(&row)?);
    }
    Ok(out)
}

/// Retrieve all blocks for a document, ordered by `seq`.
///
/// Backs `RetrievalStore::get_blocks_for_resource`. Blocks are the persisted
/// canonical source of truth for document reconstruction — see
/// `write::upsert_blocks`/`write::upsert_blocks_inner` and the trait doc on
/// `get_blocks_for_resource` in `core::store`.
pub(crate) async fn get_blocks_for_resource(
    store: &TenantStore,
    resource_id: &str,
) -> Result<Vec<localdb_core::block::Block>, Error> {
    let conn = store.conn().reader();
    let mut rows = conn
        .query(
            "SELECT seq, kind, text, metadata_json, location_json
             FROM blocks
             WHERE store_id = ? AND resource_id = ?
             ORDER BY seq",
            params![store.store_id().to_string(), resource_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_block(&row)?);
    }
    Ok(out)
}

pub(crate) async fn list_indexed_documents(
    store: &TenantStore,
) -> Result<Vec<DocumentRecord>, Error> {
    let conn = store.conn().reader();
    // `resources.id` maps back to `DocumentRecord.resource_id`. The extra
    // columns beyond the original set (metadata_json, external_id,
    // external_etag, modified_at) feed `compute_metadata_hash` below — see
    // its doc comment: this must derive from exactly the same persisted
    // state `index_resource`/`update_resource_metadata` write, or a
    // rehydrated `DocumentIndex` would disagree with the in-process one
    // about whether a resource's metadata changed (issue #176).
    let mut rows = conn
        .query(
            "SELECT id, uri, content_hash, policy_version, source_id,
                    metadata_json, external_id, external_etag, modified_at
             FROM resources WHERE store_id = ?",
            params![store.store_id().to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        let resource_id: String = row.get(0).map_err(map_libsql_err)?;
        let metadata_json: String = row.get(5).map_err(map_libsql_err)?;
        let external_id: Option<String> = row.get(6).map_err(map_libsql_err)?;
        let external_etag: Option<String> = row.get(7).map_err(map_libsql_err)?;
        let modified_at: Option<String> = row.get(8).map_err(map_libsql_err)?;

        // Deliberately re-parsed here (rather than delegating to
        // `parse_metadata_json_lenient`, the precedent `rows.rs` uses for
        // chunk reads) so the corrupt-row branch below can hash the raw
        // string instead of the lenient `Metadata::default()` fallback — see
        // that branch's comment for why.
        let metadata_hash = match serde_json::from_str::<Metadata>(&metadata_json) {
            Ok(metadata) => compute_metadata_hash(
                &metadata,
                external_id.as_deref(),
                external_etag.as_deref(),
                modified_at.as_deref(),
            ),
            Err(e) => {
                tracing::warn!(
                    resource = resource_id.as_str(),
                    error = %e,
                    "failed to parse resources.metadata_json while rehydrating the \
                     incremental-skip index; hashing the raw column instead of a \
                     default-metadata fallback so this row can never spuriously \
                     match a legitimately computed metadata_hash"
                );
                // Hashing `Metadata::default()` here would produce a real,
                // structurally valid metadata_hash indistinguishable from a
                // legitimate all-default resource — silently matching on the
                // next run and masking the corruption forever. Hashing the
                // raw (undecodable) string, tagged so it can never collide
                // with `compute_metadata_hash`'s own `\x00`-delimited
                // metadata_json-first format, instead deterministically
                // forces the next comparison to mismatch: a fresh write from
                // `update_resource_metadata`/`index_resource` self-heals the
                // row instead of the corruption hiding behind a false match.
                content_hash(&format!("\x00corrupt-metadata-json\x00{metadata_json}"))
            }
        };

        out.push(DocumentRecord {
            resource_id,
            uri: row.get(1).map_err(map_libsql_err)?,
            content_hash: row.get(2).map_err(map_libsql_err)?,
            policy_version: row.get(3).map_err(map_libsql_err)?,
            source_id: row.get(4).map_err(map_libsql_err)?,
            metadata_hash,
        });
    }
    Ok(out)
}
