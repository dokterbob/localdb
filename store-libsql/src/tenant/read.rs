use libsql::params;
use localdb_core::ingestion::DocumentRecord;
use localdb_core::{ChunkRecord, Error, MetadataFilter, SearchResult, StoreStats, VectorEncoding};

use super::rows::row_to_chunk_record_strict;
use super::TenantStore;
use crate::connection::map_libsql_err;
use crate::{build_filter_clauses, escape_fts5_query, vectors};

pub(crate) async fn dense_search(
    store: &TenantStore,
    query_vector: &[f32],
    limit: usize,
    filters: &[MetadataFilter],
) -> Result<Vec<SearchResult>, Error> {
    let conn = store.conn().conn().await;
    let filter_clauses = build_filter_clauses(filters);
    let has_filters = !filters.is_empty();
    let encoding = store.encoding();
    let dim = store.embedding_dim();
    let mut fetch_k = if has_filters { limit * 3 } else { limit };
    let max_fetch = limit * 20;

    loop {
        let qvec_sql = vectors::query_vector_sql(query_vector, encoding);
        let escaped_store_id = store.store_id().replace('\'', "''");
        let sql = format!(
            "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                    c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                    d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                    d.metadata,
                    vector_distance_cos(c.embedding, {qvec_sql}) AS distance
             FROM vector_top_k('chunks_vec_idx', {qvec_sql}, {fetch_k}) AS v
             JOIN chunks c ON c.rowid = v.id
             JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
             WHERE c.store_id = '{escaped_store_id}'
             {filter_clauses}
             ORDER BY distance ASC
             LIMIT {limit}"
        );
        let mut rows = conn.query(&sql, ()).await.map_err(map_libsql_err)?;
        let mut results = Vec::new();
        while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
            let chunk = row_to_chunk_record_strict(&row)?;
            let distance: f64 = row.get(19).map_err(map_libsql_err)?;
            let score = match encoding {
                VectorEncoding::Float32 => vectors::cosine_distance_to_score(distance),
                VectorEncoding::Binary => vectors::hamming_distance_to_score(distance, dim),
            };
            results.push(SearchResult { chunk, score });
        }
        if results.len() >= limit || !has_filters || fetch_k >= max_fetch {
            return Ok(results);
        }
        fetch_k = (fetch_k * 2).min(max_fetch);
    }
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
        "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                c.heading_path, vector_extract(c.embedding) AS embedding_json,
                d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                d.metadata,
                bm25(chunks_fts) AS score
         FROM chunks_fts f
         JOIN chunks c ON c.rowid = f.rowid
         JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
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
        let raw_score: f64 = row.get(19).map_err(map_libsql_err)?;
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
            "SELECT COUNT(*) FROM documents WHERE store_id = ?",
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
            "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                    c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                    d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                    d.metadata
             FROM chunks c
             JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
             WHERE c.store_id = ? AND c.id = ?",
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
    let mut rows = conn
        .query(
            "SELECT c.id, c.document_id, c.seq, c.text, c.span_start, c.span_end,
                    c.heading_path, vector_extract(c.embedding) AS embedding_json,
                    d.store_id, d.source_id, d.source_kind, d.uri, d.title, d.mime,
                    d.policy_version, d.fetched_at, d.content_hash, d.origin_store,
                    d.metadata
             FROM chunks c
             JOIN documents d ON d.store_id = c.store_id AND d.id = c.document_id
             WHERE c.store_id = ? AND c.document_id = ?
             ORDER BY c.seq",
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
    let mut rows = conn
        .query(
            "SELECT id, uri, content_hash, policy_version
             FROM documents WHERE store_id = ?",
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
