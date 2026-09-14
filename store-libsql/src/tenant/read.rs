use libsql::params;
use localdb_core::ingestion::DocumentRecord;
use localdb_core::{
    compute_metadata_hash, content_hash, ChunkRecord, Error, Metadata, MetadataFilter,
    ResourceRecord, SearchResult, StaleFeedResource, StoreStats, VectorEncoding,
};

use super::rows::{row_to_block, row_to_chunk_record_strict};
use super::sql::{build_filter_clauses, escape_fts5_query};
use super::{ensure_store_id, TenantStore};
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

/// Read one resource row's persisted metadata state, in exactly the shape
/// `update_resource_metadata` writes it back.
///
/// Deliberately its own projection rather than a reuse of `CHUNK_COLS`: that
/// projection omits `external_id`/`date_original`/`date_parsed` (write-only
/// on `ChunkRecord`) and also backs `dense_search`/`bm25_search`, so widening
/// it to serve this one caller would make every search row carry and parse
/// three fields no search consumer reads. See
/// `RetrievalStore::get_resource_record`.
///
/// Being a write payload rather than a display value is what sets its two
/// error rules apart from every other read here: the caller's `store_id` is
/// checked against the handle's instead of being forwarded into the `WHERE`
/// clause, and a `metadata_json` column that does not decode is an error
/// rather than a `Metadata::default()`. Both are spelled out at the point
/// they apply below.
pub(crate) async fn get_resource_record(
    store: &TenantStore,
    store_id: &str,
    resource_id: &str,
) -> Result<Option<ResourceRecord>, Error> {
    // The only read in this file that is handed a `store_id` rather than
    // taking the handle's own — the trait signature carries one because its
    // write counterpart does. Checked, never forwarded unchecked.
    ensure_store_id(store, store_id, "get_resource_record")?;
    let conn = store.conn().reader();
    let mut rows = conn
        .query(
            "SELECT metadata_json, external_id, external_etag, external_last_modified,
                    modified_at, date_original, date_parsed
             FROM resources WHERE store_id = ? AND id = ?",
            params![store_id.to_string(), resource_id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    let Some(row) = rows.next().await.map_err(map_libsql_err)? else {
        return Ok(None);
    };
    let metadata_json: String = row.get(0).map_err(map_libsql_err)?;
    // Strict, unlike the chunk-read precedent in `rows.rs`, because this is
    // not a display path: what it returns becomes the *payload* of an
    // `update_resource_metadata` call, and that rewrites every metadata
    // column of the row. A lenient `Metadata::default()` here would therefore
    // not merely misreport a corrupt row — it would overwrite whatever real
    // metadata the row still holds with an empty one, on a 304 that changed
    // nothing. Erroring leaves the row exactly as it stands and reports the
    // failure instead: `read_persisted_record` maps a store error to
    // `MetadataWriteOutcome::Failed`, which the run counts and prints. The
    // lenient helper keeps serving the read-only callers (`rows.rs`,
    // `registry/documents.rs`), where a default is a display fallback and
    // nothing is written back from it.
    let metadata: Metadata = serde_json::from_str(&metadata_json).map_err(|e| Error::Internal {
        message: format!(
            "resource '{resource_id}' has metadata_json that does not decode, so its row cannot \
             be rebuilt without destroying it: {e}"
        ),
        correlation_id: "store_handle_resource_metadata".to_string(),
    })?;
    Ok(Some(ResourceRecord {
        metadata,
        external_id: row.get(1).map_err(map_libsql_err)?,
        external_etag: row.get(2).map_err(map_libsql_err)?,
        external_last_modified: row.get(3).map_err(map_libsql_err)?,
        modified_at: row.get(4).map_err(map_libsql_err)?,
        date_original: row.get(5).map_err(map_libsql_err)?,
        date_parsed: row.get(6).map_err(map_libsql_err)?,
    }))
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
    // `external_etag`/`external_last_modified` are ALSO kept on the returned
    // `DocumentRecord` itself (not just consumed here for the hash) — they
    // are the stored conditional-GET validators
    // `IngestCallback::lookup_fetch_metadata` replays on the next fetch, and
    // this is the one place that rehydrates `DocumentIndex` across process
    // restarts.
    let mut rows = conn
        .query(
            "SELECT id, uri, content_hash, policy_version, source_id,
                    metadata_json, external_id, external_etag, modified_at,
                    external_last_modified, last_checked_at
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
        let external_last_modified: Option<String> = row.get(9).map_err(map_libsql_err)?;
        let last_checked_at: Option<String> = row.get(10).map_err(map_libsql_err)?;

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
            external_etag,
            external_last_modified,
            last_checked_at,
        });
    }
    Ok(out)
}

/// List feed liveness sweep candidates. Backs
/// `RetrievalStore::list_stale_feed_resources`.
///
/// `last_checked_at IS NULL OR last_checked_at < ?` catches both
/// never-checked rows and ones past the caller's recheck floor;
/// `ORDER BY last_checked_at ASC` alone already puts `NULL` first in SQLite
/// (verified: SQLite treats `NULL` as sorting lower than any non-`NULL`
/// value under a plain `ASC`/no explicit `NULLS FIRST|LAST`), which is
/// exactly "never-checked leading" — no `CASE`/`COALESCE` needed to spell
/// that out.
///
/// `instr(uri, '#') = 0` excludes every URI carrying a fragment, in SQL
/// rather than as a post-filter in `core::ingestion`. A link-less feed entry
/// is stored under a synthetic `{feed_url}#entry:{id}` URI
/// (specs/02-domain-model.md's "General connector pattern"); HTTP never
/// sends a fragment on the wire, so probing that URI verbatim would actually
/// request the feed root, and a 404/410 there would delete the entry's
/// resource on a signal that has nothing to do with the entry. Filtering
/// this in Rust instead would leave such rows matching the WHERE clause
/// forever — nothing ever advances their `last_checked_at` — so they would
/// keep being re-selected and re-skipped, permanently occupying slots in the
/// caller's batch cap. Excluding them here means they never become
/// candidates at all.
///
/// The accepted cost: a *real* entry link that legitimately carries a
/// fragment (`https://example.com/post#section`) is also excluded, and so
/// can never be pruned by this mechanism. That is deliberate and is the
/// correct direction to err — deletion here is asymmetric (a wrong delete
/// costs a full re-index, a missed one costs a stale hit;
/// specs/04-search-pipeline.md §1 "Deletes"), so retention bias is the safe
/// failure.
///
/// `external_id IS NOT NULL` excludes the feed's own document. In
/// single-document mode (`fetch_full_content: false`) the feed itself is
/// stored as a resource, under the feed URL, with `ingestor_kind = 'feed'` —
/// so it satisfies every other predicate here, and a 404/410 on the feed URL
/// would delete the source's entire index through a mechanism written to
/// prune one entry. Discovered entries are stamped with the entry's own id;
/// the feed root is the one feed resource that carries none, which is what
/// makes this a one-predicate separation with no new column. A legacy row
/// whose `external_id` was never captured is excluded by the same predicate
/// and can never be pruned by this mechanism — the same retention-biased
/// failure direction as the two filters above.
///
/// The scheme filter beside it is the same argument for a different shape of
/// unprobeable URI. `Uri::parse` accepts `mailto:` and `ftp:` links, and the
/// feed ingestor indexes such an entry from its embedded content under that
/// very URI. Handing one to an HTTP fetcher can only fail — never a 404/410,
/// so never a delete, but it burns one of the run's 25 probe slots on a
/// request that could not have told us anything, every run, for as long as
/// the entry is aged out. Filtered here rather than in Rust for the same
/// reason as the fragment: `last_checked_at` does advance on a transport
/// failure, so these would rotate rather than jam, but they would still
/// crowd out candidates a probe could actually resolve.
pub(crate) async fn list_stale_feed_resources(
    store: &TenantStore,
    source_id: &str,
    checked_before: &str,
    limit: usize,
) -> Result<Vec<StaleFeedResource>, Error> {
    let conn = store.conn().reader();
    let mut rows = conn
        .query(
            "SELECT id, uri, external_etag, external_last_modified
             FROM resources
             WHERE store_id = ?
               AND source_id = ?
               AND ingestor_kind = 'feed'
               AND external_id IS NOT NULL
               AND instr(uri, '#') = 0
               AND (uri LIKE 'http://%' OR uri LIKE 'https://%')
               AND (last_checked_at IS NULL OR last_checked_at < ?)
             ORDER BY last_checked_at ASC
             LIMIT ?",
            params![
                store.store_id().to_string(),
                source_id.to_string(),
                checked_before.to_string(),
                limit as i64,
            ],
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(StaleFeedResource {
            resource_id: row.get(0).map_err(map_libsql_err)?,
            uri: row.get(1).map_err(map_libsql_err)?,
            external_etag: row.get(2).map_err(map_libsql_err)?,
            external_last_modified: row.get(3).map_err(map_libsql_err)?,
        });
    }
    Ok(out)
}
