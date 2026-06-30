use libsql::params;
use localdb_core::ingestion::DocumentRecord;
use localdb_core::{ChunkRecord, Error, MetadataFilter, SearchResult, StoreStats, VectorEncoding};

use super::rows::row_to_chunk_record_strict;
use super::sql::{build_filter_clauses, escape_fts5_query};
use super::TenantStore;
use crate::connection::map_libsql_err;
use crate::vectors;

// Column projection shared across all chunk queries.
//
// The `resources` table replaces the old `documents` table. Field name mapping:
//   resources.id          → ChunkRecord.document_id
//   resources.ingestor_kind → ChunkRecord.source_kind
//   resources.added_at    → ChunkRecord.fetched_at
//   resources.metadata_json → ChunkRecord.metadata
//
// Column indices in the SELECT list (used in rows.rs):
//   0  c.id
//   1  c.resource_id      (→ document_id)
//   2  c.text
//   3  c.heading_path
//   4  vector_extract(c.embedding) AS embedding_json
//   5  r.store_id
//   6  r.source_id
//   7  r.ingestor_kind    (→ source_kind)
//   8  r.uri
//   9  r.title
//  10  r.mime
//  11  r.policy_version
//  12  r.added_at         (→ fetched_at)
//  13  r.content_hash
//  14  r.origin_store
//  15  r.metadata_json    (→ metadata)
const CHUNK_COLS: &str = "c.id, c.resource_id,
                    c.text, c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    r.store_id, r.source_id, r.ingestor_kind, r.uri, r.title, r.mime,
                    r.policy_version, r.added_at, r.content_hash, r.origin_store,
                    r.metadata_json, c.block_seq, c.seq_in_block";

pub(crate) async fn dense_search(
    store: &TenantStore,
    query_vector: &[f32],
    limit: usize,
    filters: &[MetadataFilter],
) -> Result<Vec<SearchResult>, Error> {
    let conn = store.conn().conn().await;
    let filter_clauses = build_filter_clauses(filters);
    let encoding = store.encoding();
    let dim = store.embedding_dim();
    // Always start with an overfetch multiplier: the tenant predicate
    // (WHERE c.store_id = '...') acts as a post-ANN filter even when the
    // caller supplies no MetadataFilters.
    let mut fetch_k = limit * 3;
    let max_fetch = limit * 20;

    let mut results = Vec::new();
    let mut ann_saturated = false;
    loop {
        let qvec_sql = vectors::query_vector_sql(query_vector, encoding);
        let escaped_store_id = store.store_id().replace('\'', "''");
        // TODO(#104): libsql has no partial vector indexes or ANN-level
        // predicate pushdown, so we always overfetch at the global index and
        // post-filter by store_id.  True per-store ANN partitioning would
        // require per-store chunk tables — see the tracking issue.
        // An exact-scan fallback below handles saturation by other tenants.
        let sql = format!(
            "SELECT {CHUNK_COLS},
                    vector_distance_cos(c.embedding, {qvec_sql}) AS distance
             FROM vector_top_k('chunks_vec_idx', {qvec_sql}, {fetch_k}) AS v
             JOIN chunks c ON c.rowid = v.id
             JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
             WHERE c.store_id = '{escaped_store_id}'
             {filter_clauses}
             ORDER BY distance ASC
             LIMIT {limit}"
        );
        let mut rows = conn.query(&sql, ()).await.map_err(map_libsql_err)?;
        results.clear();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            let chunk = row_to_chunk_record_strict(&row)?;
            let distance: f64 = row.get(18).map_err(map_libsql_err)?;
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
        let escaped_store_id = store.store_id().replace('\'', "''");
        let sql = format!(
            "SELECT {CHUNK_COLS},
                    vector_distance_cos(c.embedding, {qvec_sql}) AS distance
             FROM chunks c
             JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
             WHERE c.store_id = '{escaped_store_id}'
             {filter_clauses}
             ORDER BY distance ASC
             LIMIT {limit}"
        );
        let mut rows = conn.query(&sql, ()).await.map_err(map_libsql_err)?;
        results.clear();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            let chunk = row_to_chunk_record_strict(&row)?;
            let distance: f64 = row.get(18).map_err(map_libsql_err)?;
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
    let conn = store.conn().conn().await;
    let escaped_query = escape_fts5_query(query_text);
    let filter_clauses = build_filter_clauses(filters);
    let escaped_store_id = store.store_id().replace('\'', "''");
    let sql = format!(
        "SELECT {CHUNK_COLS},
                bm25(chunks_fts) AS score
         FROM chunks_fts f
         JOIN chunks c ON c.rowid = f.rowid
         JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
         WHERE chunks_fts MATCH ?
         AND c.store_id = '{escaped_store_id}'
         {filter_clauses}
         ORDER BY score ASC
         LIMIT {limit}"
    );
    let mut rows = conn
        .query(&sql, params![escaped_query])
        .await
        .map_err(map_libsql_err)?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        let chunk = row_to_chunk_record_strict(&row)?;
        let raw_score: f64 = row.get(18).map_err(map_libsql_err)?;
        results.push(SearchResult {
            chunk,
            score: -raw_score as f32,
        });
    }
    Ok(results)
}

pub(crate) async fn stats(store: &TenantStore) -> Result<StoreStats, Error> {
    let conn = store.conn().conn().await;
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM chunks WHERE store_id = ?",
            params![store.store_id().to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let chunk_count = match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row.get::<u64>(0).map_err(map_libsql_err)?,
        None => 0,
    };
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM resources WHERE store_id = ?",
            params![store.store_id().to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let document_count = match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row.get::<u64>(0).map_err(map_libsql_err)?,
        None => 0,
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
    let conn = store.conn().conn().await;
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

pub(crate) async fn get_chunks_for_document(
    store: &TenantStore,
    document_id: &str,
) -> Result<Vec<ChunkRecord>, Error> {
    let conn = store.conn().conn().await;
    // `document_id` maps to `resource_id` in the new schema.
    let mut rows = conn
        .query(
            &format!(
                "SELECT {CHUNK_COLS}
                 FROM chunks c
                 JOIN resources r ON r.store_id = c.store_id AND r.id = c.resource_id
                 WHERE c.store_id = ? AND c.resource_id = ?
                 ORDER BY c.block_seq, c.seq_in_block"
            ),
            params![store.store_id().to_string(), document_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_chunk_record_strict(&row)?);
    }
    Ok(out)
}

pub(crate) async fn list_indexed_documents(
    store: &TenantStore,
) -> Result<Vec<DocumentRecord>, Error> {
    let conn = store.conn().conn().await;
    // `resources.id` maps back to `DocumentRecord.document_id`.
    let mut rows = conn
        .query(
            "SELECT id, uri, content_hash, policy_version
             FROM resources WHERE store_id = ?",
            params![store.store_id().to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(DocumentRecord {
            document_id: row.get(0).map_err(map_libsql_err)?,
            uri: row.get(1).map_err(map_libsql_err)?,
            content_hash: row.get(2).map_err(map_libsql_err)?,
            policy_version: row.get(3).map_err(map_libsql_err)?,
        });
    }
    Ok(out)
}
