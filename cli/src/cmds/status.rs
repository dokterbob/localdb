use std::path::{Path, PathBuf};

use serde_json::json;

use crate::{
    app_db::{load_app_db_lenient, resolve_store_scope, AppDb, StoreScopePolicy},
    daemon_client::{probe_daemon, CliContext, DaemonState},
    normalize::{print_json, visibility_to_string},
};

/// How many rows `StoreBackend::largest_tables` is asked for; matches the
/// number surfaced in both `--json` and human output.
const LARGEST_TABLES_LIMIT: usize = 5;

/// Per-store figures gathered for `status`'s output.
///
/// `stats` is `None` when `RetrievalStore::stats()` itself failed for this
/// store (e.g. a corrupt or mid-migration store) — `status` must keep
/// reporting on the daemon state and the other stores rather than aborting
/// outright; see `gather_store_status`.
pub(crate) struct StoreStatusEntry {
    pub name: String,
    pub visibility: &'static str,
    pub backend: String,
    pub stats: Option<localdb_core::StoreStats>,
}

/// On-disk size of the single unified `localdb.db` file shared by every
/// store, plus its `-wal` sidecar.
///
/// specs/03-config.md: there is exactly one physical file for the whole
/// database — file size is never a per-store figure, so `status` reports it
/// once, in a top-level `database` section, not attached to any one store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DbFileSize {
    /// Bytes in `localdb.db` itself. `None` if the file doesn't exist yet,
    /// or a stat error of any kind — `status` degrades to "unknown" rather
    /// than failing (specs/05-surfaces.md).
    pub main_bytes: Option<u64>,
    /// Bytes in the `-wal` sidecar, if one exists.
    ///
    /// Deliberately included in `total_bytes`, not just `main_bytes`: WAL
    /// mode (set by `LibsqlDb::open`) defers committed pages there until the
    /// next checkpoint, so on a store with recent writes a large share of
    /// genuine on-disk usage can live in the WAL rather than the main file.
    /// Reporting `main_bytes` alone would understate current disk usage —
    /// exactly the blind spot issues #179/#177 are about.
    pub wal_bytes: Option<u64>,
}

impl DbFileSize {
    /// `main_bytes + wal_bytes`, treating a missing file/sidecar as 0 bytes.
    pub(crate) fn total_bytes(&self) -> u64 {
        self.main_bytes.unwrap_or(0) + self.wal_bytes.unwrap_or(0)
    }
}

/// Stat `db_path` and its `-wal` sidecar.
///
/// Never fails: any stat error (missing file, permissions, ...) degrades the
/// corresponding field to `None` rather than propagating — `status` must
/// keep working even before the database file exists.
pub(crate) fn compute_db_file_size(db_path: &Path) -> DbFileSize {
    let main_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());

    let mut wal_name = db_path.as_os_str().to_owned();
    wal_name.push("-wal");
    let wal_path = PathBuf::from(wal_name);
    let wal_bytes = std::fs::metadata(&wal_path).ok().map(|m| m.len());

    DbFileSize {
        main_bytes,
        wal_bytes,
    }
}

/// Sum of `chunk_count` across every store whose stats were available.
fn total_chunk_count(stores: &[StoreStatusEntry]) -> u64 {
    stores
        .iter()
        .filter_map(|s| s.stats.as_ref())
        .map(|s| s.chunk_count)
        .sum()
}

/// `total on-disk bytes / total chunks` — the single number that makes an
/// over-sized index obvious at a glance, which is the whole point of this
/// diagnostic (issues #179, #177). `None` when there are no chunks to divide
/// by (avoids a division by zero and a meaningless "0 bytes/chunk").
fn bytes_per_chunk(total_bytes: u64, total_chunks: u64) -> Option<u64> {
    total_bytes.checked_div(total_chunks)
}

/// Human-readable byte size, e.g. `128.4 MB`. Binary (1024) units, matching
/// the `du -h` convention.
pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Fetch `RetrievalStore::stats()` for every store in scope, tolerating a
/// per-store failure — a single corrupt or mid-migration store must not blank
/// out `status`'s report on the rest.
pub(crate) async fn gather_store_status(
    db: &AppDb,
    runtime_stores: &[localdb_core::StoreRow],
) -> Vec<StoreStatusEntry> {
    let mut out = Vec::with_capacity(runtime_stores.len());
    for s in runtime_stores {
        let stats = match db.backend().retrieval_store(&s.id).await {
            Ok(store) => store.stats().await.ok(),
            Err(_) => None,
        };
        out.push(StoreStatusEntry {
            name: s.name.clone(),
            visibility: visibility_to_string(&s.visibility),
            backend: s.backend.clone(),
            stats,
        });
    }
    out
}

/// Build the `--json` payload.
///
/// A pure function of already-gathered data so it's testable without a real
/// store, filesystem, or daemon probe. Extends the pre-existing shape
/// (`daemon`, `stores[].{name,visibility,backend}`) rather than replacing
/// it: existing consumers of those fields see no change.
pub(crate) fn build_status_json(
    daemon_status: &str,
    stores: &[StoreStatusEntry],
    db_path: &Path,
    db_size: DbFileSize,
    largest_tables: &[localdb_core::TableSize],
) -> serde_json::Value {
    let store_json: Vec<serde_json::Value> = stores
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "visibility": s.visibility,
                "backend": s.backend,
                "document_count": s.stats.as_ref().map(|st| st.document_count),
                "chunk_count": s.stats.as_ref().map(|st| st.chunk_count),
            })
        })
        .collect();

    let total_bytes = db_size.total_bytes();
    let total_chunks = total_chunk_count(stores);
    let tables_json: Vec<serde_json::Value> = largest_tables
        .iter()
        .map(|t| json!({ "name": t.name, "bytes": t.bytes }))
        .collect();

    json!({
        "daemon": daemon_status,
        "stores": store_json,
        // A single unified database file backs every store above
        // (specs/03-config.md) — these figures describe the *file*, not any
        // one store, and are therefore reported once here rather than
        // per-store.
        "database": {
            "path": db_path.display().to_string(),
            "exists": db_size.main_bytes.is_some(),
            "size_bytes": db_size.main_bytes,
            "wal_size_bytes": db_size.wal_bytes,
            "total_size_bytes": total_bytes,
            "bytes_per_chunk": bytes_per_chunk(total_bytes, total_chunks),
            "largest_tables": tables_json,
        },
    })
}

/// Print the human-readable form of the same data `build_status_json` emits.
pub(crate) fn print_status_human(
    daemon_status: &str,
    stores: &[StoreStatusEntry],
    db_path: &Path,
    db_size: DbFileSize,
    largest_tables: &[localdb_core::TableSize],
) {
    println!("daemon: {}", daemon_status);
    println!("stores ({}):", stores.len());
    if stores.is_empty() {
        println!("  (none)");
    }
    for s in stores {
        match &s.stats {
            Some(stats) => println!(
                "  {} [{}] {} documents, {} chunks",
                s.name, s.backend, stats.document_count, stats.chunk_count
            ),
            None => println!("  {} [{}] (stats unavailable)", s.name, s.backend),
        }
    }

    println!();
    println!("database: {}", db_path.display());
    match db_size.main_bytes {
        Some(bytes) => {
            print!("  size: {}", format_bytes(bytes));
            if let Some(wal) = db_size.wal_bytes {
                print!(" (+ {} WAL)", format_bytes(wal));
            }
            println!();
        }
        None => println!("  size: unknown (file not found)"),
    }

    let total_chunks = total_chunk_count(stores);
    if let Some(bpc) = bytes_per_chunk(db_size.total_bytes(), total_chunks) {
        println!(
            "  ~{} per chunk ({} chunks total)",
            format_bytes(bpc),
            total_chunks
        );
    }

    if !largest_tables.is_empty() {
        println!("  largest tables:");
        for t in largest_tables {
            println!("    {} — {}", t.name, format_bytes(t.bytes));
        }
    }
}

/// `localdb status`
pub fn run_status(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_status_async(ctx));
}

pub(crate) async fn run_status_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so status works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx).await;
    let data_dir = &config_loader.paths.data_dir;
    let db_path = config_loader.paths.db_path();

    let daemon_status = match probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        DaemonState::Running { base_url } => format!("running ({})", base_url),
        DaemonState::NotRunning => "not running (embedded mode)".to_string(),
    };

    // specs/05-surfaces.md §2.2: `--store` is repeatable and always validated
    // and resolved; the "all stores" behavior only applies when `-s` is
    // omitted. Route through the shared resolver rather than listing
    // everything unconditionally.
    let runtime_stores = resolve_store_scope(ctx, &db, StoreScopePolicy::AllStores).await;
    let stores = gather_store_status(&db, &runtime_stores).await;

    let db_size = compute_db_file_size(&db_path);
    // Best-effort diagnostic (see `StoreBackend::largest_tables`'s doc
    // comment) — an error here must not take `status` down with it.
    let largest_tables = db
        .backend()
        .largest_tables(LARGEST_TABLES_LIMIT)
        .await
        .unwrap_or_default();

    if ctx.json {
        print_json(&build_status_json(
            &daemon_status,
            &stores,
            &db_path,
            db_size,
            &largest_tables,
        ));
    } else {
        print_status_human(&daemon_status, &stores, &db_path, db_size, &largest_tables);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::config::schema::{
        DefaultsConfig, EmbeddingPolicy, PathsConfig, RawConfig, ServerConfig,
    };
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
            version: 1,
            server: ServerConfig::default(),
            paths: PathsConfig::default(),
            defaults,
            providers: vec![],
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
}
