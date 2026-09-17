use super::*;
use localdb_core::config::schema::{DefaultsConfig, EmbeddingPolicy, RawConfig};
use localdb_core::{SourceKind, SourceRow, StoreRow, TableSize};
use tempfile::TempDir;

// -----------------------------------------------------------------------
// compute_db_file_size
// -----------------------------------------------------------------------

#[test]
fn compute_db_file_size_on_missing_file_is_all_none() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("does-not-exist.db");
    let size = compute_db_file_size(&path);
    assert_eq!(size.main_bytes, None);
    assert_eq!(size.wal_bytes, None);
    assert_eq!(size.total_bytes(), 0);
}

#[test]
fn compute_db_file_size_reports_main_file_len() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("localdb.db");
    std::fs::write(&path, vec![0u8; 1234]).unwrap();
    let size = compute_db_file_size(&path);
    assert_eq!(size.main_bytes, Some(1234));
    assert_eq!(size.wal_bytes, None);
    assert_eq!(size.total_bytes(), 1234);
}

#[test]
fn compute_db_file_size_includes_wal_sidecar_in_total() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("localdb.db");
    std::fs::write(&path, vec![0u8; 1000]).unwrap();
    let wal_path = dir.path().join("localdb.db-wal");
    std::fs::write(&wal_path, vec![0u8; 500]).unwrap();

    let size = compute_db_file_size(&path);
    assert_eq!(size.main_bytes, Some(1000));
    assert_eq!(size.wal_bytes, Some(500));
    assert_eq!(
        size.total_bytes(),
        1500,
        "total must include the WAL sidecar, not just the main file"
    );
}

// -----------------------------------------------------------------------
// format_bytes
// -----------------------------------------------------------------------

#[test]
fn format_bytes_covers_all_magnitudes() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1536), "1.5 KB");
    assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    assert_eq!(format_bytes(45 * 1024 * 1024 * 1024), "45.0 GB");
}

// -----------------------------------------------------------------------
// bytes_per_chunk
// -----------------------------------------------------------------------

#[test]
fn bytes_per_chunk_none_when_no_chunks() {
    assert_eq!(bytes_per_chunk(1_000_000, 0), None);
}

#[test]
fn bytes_per_chunk_divides_total_by_count() {
    assert_eq!(bytes_per_chunk(1_000, 10), Some(100));
}

// -----------------------------------------------------------------------
// build_status_json
// -----------------------------------------------------------------------

fn entry_with_stats(name: &str, doc_count: u64, chunk_count: u64) -> StoreStatusEntry {
    StoreStatusEntry {
        name: name.to_string(),
        visibility: "private",
        backend: "libsql".to_string(),
        stats: Some(localdb_core::StoreStats {
            document_count: doc_count,
            chunk_count,
        }),
    }
}

#[test]
fn build_status_json_preserves_pre_existing_fields() {
    let stores = vec![entry_with_stats("notes", 3, 30)];
    let value = build_status_json(
        "not running (embedded mode)",
        &stores,
        Path::new("/data/localdb.db"),
        DbFileSize {
            main_bytes: Some(1024),
            wal_bytes: None,
        },
        &[],
    );

    // Pre-existing shape: daemon + stores[].{name,visibility,backend}
    // must still be present and typed exactly as before.
    assert_eq!(value["daemon"], "not running (embedded mode)");
    assert_eq!(value["stores"][0]["name"], "notes");
    assert_eq!(value["stores"][0]["visibility"], "private");
    assert_eq!(value["stores"][0]["backend"], "libsql");
}

#[test]
fn build_status_json_adds_per_store_counts() {
    let stores = vec![entry_with_stats("notes", 3, 30)];
    let value = build_status_json(
        "not running (embedded mode)",
        &stores,
        Path::new("/data/localdb.db"),
        DbFileSize {
            main_bytes: Some(3000),
            wal_bytes: None,
        },
        &[],
    );

    assert_eq!(value["stores"][0]["document_count"], 3);
    assert_eq!(value["stores"][0]["chunk_count"], 30);
}

#[test]
fn build_status_json_reports_null_counts_when_stats_unavailable() {
    let stores = vec![StoreStatusEntry {
        name: "broken".to_string(),
        visibility: "private",
        backend: "libsql".to_string(),
        stats: None,
    }];
    let value = build_status_json(
        "not running (embedded mode)",
        &stores,
        Path::new("/data/localdb.db"),
        DbFileSize::default(),
        &[],
    );

    assert!(value["stores"][0]["document_count"].is_null());
    assert!(value["stores"][0]["chunk_count"].is_null());
}

#[test]
fn build_status_json_reports_file_backed_size_not_per_store() {
    let stores = vec![entry_with_stats("a", 1, 10), entry_with_stats("b", 1, 90)];
    let db_size = DbFileSize {
        main_bytes: Some(900),
        wal_bytes: Some(100),
    };
    let value = build_status_json(
        "not running (embedded mode)",
        &stores,
        Path::new("/data/localdb.db"),
        db_size,
        &[],
    );

    // The database section is a single object describing the shared
    // file, not an array keyed by store.
    assert_eq!(value["database"]["path"], "/data/localdb.db");
    assert_eq!(value["database"]["exists"], true);
    assert_eq!(value["database"]["size_bytes"], 900);
    assert_eq!(value["database"]["wal_size_bytes"], 100);
    assert_eq!(value["database"]["total_size_bytes"], 1000);
    // 1000 bytes / 100 total chunks (10 + 90) = 10 bytes/chunk.
    assert_eq!(value["database"]["bytes_per_chunk"], 10);
}

#[test]
fn build_status_json_bytes_per_chunk_is_null_with_no_chunks() {
    let value = build_status_json(
        "not running (embedded mode)",
        &[],
        Path::new("/data/localdb.db"),
        DbFileSize {
            main_bytes: Some(500),
            wal_bytes: None,
        },
        &[],
    );
    assert!(value["database"]["bytes_per_chunk"].is_null());
}

#[test]
fn build_status_json_missing_file_reports_exists_false_and_null_size() {
    let value = build_status_json(
        "not running (embedded mode)",
        &[],
        Path::new("/data/localdb.db"),
        DbFileSize::default(),
        &[],
    );
    assert_eq!(value["database"]["exists"], false);
    assert!(value["database"]["size_bytes"].is_null());
}

#[test]
fn build_status_json_includes_largest_tables() {
    let tables = vec![
        TableSize {
            name: "chunks".to_string(),
            bytes: 900,
        },
        TableSize {
            name: "resources".to_string(),
            bytes: 100,
        },
    ];
    let value = build_status_json(
        "not running (embedded mode)",
        &[],
        Path::new("/data/localdb.db"),
        DbFileSize::default(),
        &tables,
    );
    assert_eq!(value["database"]["largest_tables"][0]["name"], "chunks");
    assert_eq!(value["database"]["largest_tables"][0]["bytes"], 900);
    assert_eq!(value["database"]["largest_tables"][1]["name"], "resources");
}

// -----------------------------------------------------------------------
// gather_store_status — exercised against a real (tempdir-backed) AppDb,
// matching the pattern used by app_db.rs's own tests.
// -----------------------------------------------------------------------

async fn tmp_app_db(dir: &TempDir) -> AppDb {
    let mut defaults = DefaultsConfig::default();
    defaults.indexing.embedding = EmbeddingPolicy {
        provider: "fake".into(),
        model: "default".into(),
    };
    let config = RawConfig {
        defaults,
        ..Default::default()
    };
    let paths = localdb_core::config::loader::ResolvedPaths {
        config_file: dir.path().join("config.yaml"),
        data_dir: dir.path().to_path_buf(),
        models_dir: dir.path().join("models"),
        logs_dir: dir.path().join("logs"),
    };
    AppDb::open(
        &paths,
        &config.defaults.indexing.embedding,
        &config.providers,
        config.defaults.indexing.clone(),
    )
    .await
    .unwrap()
}

fn test_store_row(name: &str, db: &AppDb) -> StoreRow {
    crate::app_db::default_store_row(name, db).unwrap()
}

fn test_source_row(store_id: &str) -> SourceRow {
    SourceRow {
        id: localdb_core::ids::new_ulid(),
        store_id: store_id.to_string(),
        kind: SourceKind::Path,
        root: Some("/docs".to_string()),
        url: None,
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: "2026-06-25T12:00:00Z".to_string(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    }
}

fn test_chunk(id: &str, store_id: &str, source_id: &str) -> localdb_core::ChunkRecord {
    localdb_core::ChunkRecord {
        id: id.to_string(),
        resource_id: "doc-1".to_string(),
        store_id: store_id.to_string(),
        text: "hello world".to_string(),
        span: localdb_core::types::Span::new(0, 11),
        heading_path: vec![],
        // `tmp_app_db`'s "fake"/"default" embedding policy resolves to a
        // 128-dim embedder (see `embed::factory::SHAPES`) — the vector
        // length here must match or `upsert_chunks` rejects it.
        embedding: vec![0.1; 128],
        policy_version: "v1".to_string(),
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        modified_at: Some("2026-06-10T12:00:00Z".to_string()),
        content_hash: "abc123".to_string(),
        origin_store: store_id.to_string(),
        source_id: source_id.to_string(),
        ingestor_kind: "path".to_string(),
        mime: Some("text/markdown".to_string()),
        uri: "file:///docs/doc.md".to_string(),
        metadata: Default::default(),
        block_seq: 0,
        seq_in_block: 0,
        block_kind: None,
        page: None,
        window_block_seqs: vec![],
        date_original: None,
        date_parsed: None,
        external_id: None,
        external_etag: None,
    }
}

#[tokio::test]
async fn gather_store_status_reports_zero_counts_for_an_empty_store() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let store = test_store_row("empty", &db);
    db.backend().upsert_store(&store).await.unwrap();

    let rows = db.backend().list_stores().await.unwrap();
    let stores = gather_store_status(&db, &rows).await;

    assert_eq!(stores.len(), 1);
    let stats = stores[0].stats.as_ref().expect("stats must be available");
    assert_eq!(stats.document_count, 0);
    assert_eq!(stats.chunk_count, 0);
}

#[tokio::test]
async fn gather_store_status_reflects_real_chunk_and_document_counts() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let store = test_store_row("notes", &db);
    db.backend().upsert_store(&store).await.unwrap();
    let source = test_source_row(&store.id);
    db.backend().upsert_source(&source).await.unwrap();

    let handle = db.backend().retrieval_store(&store.id).await.unwrap();
    handle
        .upsert_chunks(vec![
            test_chunk("c1", &store.id, &source.id),
            test_chunk("c2", &store.id, &source.id),
        ])
        .await
        .unwrap();

    let rows = db.backend().list_stores().await.unwrap();
    let stores = gather_store_status(&db, &rows).await;

    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].name, "notes");
    let stats = stores[0].stats.as_ref().unwrap();
    assert_eq!(stats.chunk_count, 2);
    assert_eq!(stats.document_count, 1);
}

#[tokio::test]
async fn gather_store_status_covers_multiple_stores_independently() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;

    let a = test_store_row("a", &db);
    db.backend().upsert_store(&a).await.unwrap();
    let src_a = test_source_row(&a.id);
    db.backend().upsert_source(&src_a).await.unwrap();
    db.backend()
        .retrieval_store(&a.id)
        .await
        .unwrap()
        .upsert_chunks(vec![test_chunk("a1", &a.id, &src_a.id)])
        .await
        .unwrap();

    let b = test_store_row("b", &db);
    db.backend().upsert_store(&b).await.unwrap();

    let rows = db.backend().list_stores().await.unwrap();
    let stores = gather_store_status(&db, &rows).await;

    let a_entry = stores.iter().find(|s| s.name == "a").unwrap();
    let b_entry = stores.iter().find(|s| s.name == "b").unwrap();
    assert_eq!(a_entry.stats.as_ref().unwrap().chunk_count, 1);
    assert_eq!(b_entry.stats.as_ref().unwrap().chunk_count, 0);
}
