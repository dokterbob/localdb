//! Pins the **measured** per-row cost of the `chunks_vec_idx` DiskANN index
//! (issues #179, #177).
//!
//! Issue #179 reported 45 GB for ~600k chunks whose raw vectors are under
//! 1 GB. The cause was index tuning, not duplicated data: libsql allocates
//! every DiskANN node as a fixed-size blob
//!
//! ```text
//! block_size = (node_vec_size + 16) + max_neighbors × (edge_vec_size + 16)
//! ```
//!
//! and v5 pinned `compress_neighbors=float8` on an `F1BIT_BLOB` column, so
//! each 1-bit node vector's neighbors were stored a *byte* per dimension —
//! 8× the space for information the node vector already held.
//!
//! These assertions read libsql's own `libsql_vector_meta_shadow` rather than
//! recomputing the formula, so they fail if a libsql upgrade changes the
//! defaults out from under `vectors::vector_index_params` — the exact silent
//! regression that produced #179.

use libsql::{Builder, Connection};
use localdb_core::{StoreBackend, StoreBackendConfig, VectorEncoding};
use store_libsql::SqliteBackend;
use tempfile::tempdir;

/// The v5 tuning, for contrast. Frozen — this is what we migrated away from.
const V5_PARAMS: &str = "'metric=cosine', 'max_neighbors=64', 'compress_neighbors=float8'";

/// `libsql_vector_meta_shadow.metadata` is a sequence of 9-byte records, each
/// a `u8` tag followed by a little-endian `u64`.
const TAG_BLOCK_SIZE: u8 = 0x06;
const TAG_MAX_NEIGHBORS: u8 = 0x0A;

async fn open() -> (tempfile::TempDir, Connection) {
    let dir = tempdir().unwrap();
    let db = Builder::new_local(dir.path().join("test.db"))
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    (dir, conn)
}

/// Create a store through the **production** open path (`SqliteBackend::open`
/// → `schema::create_schema`), then read its index metadata back over a plain
/// libsql connection. Going through the real entry point is the point: it's
/// what a `localdb init` actually builds.
async fn open_real_store(
    dim: usize,
    encoding: VectorEncoding,
) -> (tempfile::TempDir, SqliteBackend, Connection) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(path.clone(), dim, encoding))
        .await
        .unwrap();
    let db = Builder::new_local(&path).build().await.unwrap();
    let conn = db.connect().unwrap();
    (dir, backend, conn)
}

async fn index_meta(conn: &Connection) -> Vec<(u8, u64)> {
    let mut rows = conn
        .query(
            "SELECT metadata FROM libsql_vector_meta_shadow WHERE name = 'chunks_vec_idx'",
            (),
        )
        .await
        .unwrap();
    let blob: Vec<u8> = rows.next().await.unwrap().unwrap().get(0).unwrap();
    blob.chunks_exact(9)
        .map(|r| (r[0], u64::from_le_bytes(r[1..9].try_into().unwrap())))
        .collect()
}

fn tag(meta: &[(u8, u64)], want: u8) -> Option<u64> {
    meta.iter().find(|(t, _)| *t == want).map(|(_, v)| *v)
}

/// Build a bare `chunks(embedding)` table + index with explicit params, so a
/// test can measure a tuning that no longer appears anywhere in the schema.
async fn build_with_params(conn: &Connection, col_type: &str, params: &str) {
    conn.execute(
        &format!("CREATE TABLE chunks (rowid_ INTEGER PRIMARY KEY, embedding {col_type} NOT NULL)"),
        (),
    )
    .await
    .unwrap();
    conn.execute(
        &format!("CREATE INDEX chunks_vec_idx ON chunks(libsql_vector_idx(embedding, {params}))"),
        (),
    )
    .await
    .unwrap();
}

/// The headline number: a binary store's per-chunk index cost, as libsql
/// itself reports it.
///
/// 7,488 = (128 + 16) + 51 × (128 + 16). The 51 is libsql's own default,
/// derived from its "cap edge overhead at 50× node overhead" rule — we do not
/// pin `max_neighbors`, so this asserts the rule still lands where the sizing
/// in issue #179 assumed it does.
#[tokio::test]
async fn binary_index_costs_7488_bytes_per_chunk() {
    let (_dir, _backend, conn) = open_real_store(1024, VectorEncoding::Binary).await;

    let meta = index_meta(&conn).await;
    assert_eq!(
        tag(&meta, TAG_BLOCK_SIZE),
        Some(7_488),
        "binary DiskANN block size changed; at 600k chunks each extra byte is 600 KB \
         and issue #179's sizing no longer holds. meta = {meta:?}"
    );
    assert_eq!(
        tag(&meta, TAG_MAX_NEIGHBORS),
        None,
        "max_neighbors must stay unpinned for binary columns so libsql applies its own \
         50x-disk-overhead rule (which is what yields the 51 edges in the 7,488 figure)"
    );
}

/// The regression this change fixes, measured rather than argued: the v5
/// tuning on the same column is 9.0× larger.
#[tokio::test]
async fn v5_binary_tuning_was_9x_larger() {
    let (_dir, conn) = open().await;
    build_with_params(&conn, "F1BIT_BLOB(1024)", V5_PARAMS).await;

    let v5 = tag(&index_meta(&conn).await, TAG_BLOCK_SIZE).unwrap();
    assert_eq!(v5, 67_216, "v5's measured per-chunk cost");
    assert_eq!(
        v5 / 7_488,
        8,
        "v5/v6 ratio (integer division of 8.98x) — if this moves, re-check the \
         GB figures quoted in specs/04-search-pipeline.md"
    );
}

/// The v6 migration's central claim: `DROP INDEX` + `CREATE INDEX` really
/// **refills** the index from `chunks.embedding`, rather than leaving an empty
/// one behind that silently returns no search results.
///
/// This is the failure mode that would be invisible in a schema-shape test and
/// catastrophic in production — a store that migrates "successfully" and then
/// answers every query with nothing. So assert on rows and on retrievability,
/// not on `sqlite_master`.
///
/// It also demonstrates that no re-embedding is involved: nothing here has an
/// embedder, and the rebuild is driven entirely off the stored column.
#[tokio::test]
async fn rebuilding_the_index_refills_it_from_the_stored_embeddings() {
    let (_dir, conn) = open().await;
    build_with_params(&conn, "F1BIT_BLOB(1024)", V5_PARAMS).await;

    // 300 distinct vectors, each differing from the last in one bit region.
    const N: usize = 300;
    for i in 0..N {
        conn.execute(
            &format!(
                "INSERT INTO chunks (rowid_, embedding) VALUES ({i}, {})",
                bit_vector_literal(i)
            ),
            (),
        )
        .await
        .unwrap();
    }
    assert_eq!(shadow_rows(&conn).await, N as i64);
    assert_neighbours_are_genuinely_near(&conn, 7).await;

    // Exactly the statements `chain::shrink_vector_index_up` emits.
    conn.execute("DROP INDEX IF EXISTS chunks_vec_idx", ())
        .await
        .unwrap();
    conn.execute(
        "CREATE INDEX IF NOT EXISTS chunks_vec_idx ON chunks(\
         libsql_vector_idx(embedding, 'metric=cosine'))",
        (),
    )
    .await
    .unwrap();

    assert_eq!(
        shadow_rows(&conn).await,
        N as i64,
        "CREATE INDEX must refill every row — an empty index would leave the store \
         silently unsearchable after a 'successful' migration"
    );
    assert_eq!(
        tag(&index_meta(&conn).await, TAG_BLOCK_SIZE),
        Some(7_488),
        "and the rebuilt index must actually carry the new, smaller tuning"
    );
    assert_neighbours_are_genuinely_near(&conn, 7).await;
}

/// Assert the index returns *genuinely near* neighbours for `query`.
///
/// Deliberately NOT "the same ids as before the rebuild". DiskANN graph
/// construction is not deterministic — the traversal's start node and
/// tie-breaking are unseeded — so an equality assertion against a previous
/// run's result set tests graph topology rather than correctness, and flakes.
/// (Observed: an identical rebuild-then-query scenario returning a completely
/// different id cluster on some runs at larger N.)
///
/// Instead assert the property that actually matters and is stable: the exact
/// match comes back, and nothing wildly distant does. With
/// [`bit_vector_literal`]'s construction, `hamming(i, j) == |i - j|`, so
/// "distance" here is just id distance — which makes the bound exact rather
/// than a guess. This still catches the failure mode that matters (an index
/// that rebuilt empty, or one returning an unrelated region of the space)
/// without asserting an unstable graph.
async fn assert_neighbours_are_genuinely_near(conn: &Connection, query: usize) {
    const K: usize = 5;
    // K nearest to `query` are the K ids closest to it, so the furthest any
    // correct result can be is K (bounded by the one-sided case at an edge of
    // the id range). Allow 2x that as slack for ANN inexactness.
    const MAX_DISTANCE: i64 = 2 * K as i64;

    let hits = top_k_ids(conn, query, K).await;
    assert_eq!(hits.len(), K, "index must return k results, got {hits:?}");
    assert!(
        hits.contains(&(query as i64)),
        "the query vector is present verbatim in the corpus, so it must be its \
         own nearest neighbour; got {hits:?}"
    );
    for id in &hits {
        assert!(
            (id - query as i64).abs() <= MAX_DISTANCE,
            "id {id} is {} away from query {query} — the index is returning an \
             unrelated region of the space, not near neighbours; got {hits:?}",
            (id - query as i64).abs()
        );
    }
}

/// A deterministic, well-separated 1024-bit vector: `i` leading 1-bits, rest 0.
/// Hamming distance between `i` and `j` is exactly `|i - j|`, so a query's
/// nearest neighbours are unambiguous and a recall comparison is meaningful.
fn bit_vector_literal(i: usize) -> String {
    let mut s = String::from("vector1bit('[");
    for bit in 0..1024 {
        if bit > 0 {
            s.push(',');
        }
        s.push(if bit < i { '1' } else { '0' });
    }
    s.push_str("]')");
    s
}

async fn shadow_rows(conn: &Connection) -> i64 {
    let mut rows = conn
        .query("SELECT COUNT(*) FROM chunks_vec_idx_shadow", ())
        .await
        .unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

async fn top_k_ids(conn: &Connection, query: usize, k: usize) -> Vec<i64> {
    let mut rows = conn
        .query(
            &format!(
                // `vector_top_k` exposes the matched rowids as a column named
                // `id`, regardless of what the base table calls its own key.
                "SELECT id FROM vector_top_k('chunks_vec_idx', {}, {k})",
                bit_vector_literal(query)
            ),
            (),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(r) = rows.next().await.unwrap() {
        out.push(r.get(0).unwrap());
    }
    out
}

/// Float32 keeps both params, and this is why: with them omitted libsql
/// defaults the edge type to the node type — 4 KiB float32 edges — landing 3×
/// *worse* than the tuning we keep. The invariant is "`compress_neighbors`
/// must never be wider than the column's own encoding", not "never set it".
#[tokio::test]
async fn float32_keeps_float8_neighbors_because_bare_defaults_are_worse() {
    let (_dir, _backend, tuned) = open_real_store(1024, VectorEncoding::Float32).await;
    let tuned_size = tag(&index_meta(&tuned).await, TAG_BLOCK_SIZE).unwrap();
    assert_eq!(tuned_size, 71_184);
    assert_eq!(
        tag(&index_meta(&tuned).await, TAG_MAX_NEIGHBORS),
        Some(64),
        "float32 must keep max_neighbors pinned"
    );

    let (_dir2, bare) = open().await;
    build_with_params(&bare, "F32_BLOB(1024)", "'metric=cosine'").await;
    let bare_size = tag(&index_meta(&bare).await, TAG_BLOCK_SIZE).unwrap();
    assert_eq!(bare_size, 213_824);
    assert!(
        bare_size > tuned_size * 2,
        "dropping the params on a float32 column must be recognised as a regression, \
         not copied from the binary path: bare={bare_size} tuned={tuned_size}"
    );
}
