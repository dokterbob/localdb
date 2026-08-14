use std::path::{Path, PathBuf};

use localdb_core::config::loader::ConfigLoader;
use localdb_core::{bytes_per_chunk, compute_db_file_size, format_bytes, DbFileSize, Error};
use serde_json::json;

use crate::{
    app_db::{
        apply_daemon_store_scope, load_config_lenient, open_app_db_lenient_or_exit,
        resolve_store_scope_inner, AppDb, StoreScopePolicy,
    },
    command_table::{dispatch, DaemonAwareCommand},
    daemon_client::{daemon_request_async, encode_path_segment, CliContext},
    normalize::{print_json, visibility_to_string},
};

/// How many rows `StoreBackend::largest_tables` is asked for; matches the
/// number surfaced in both `--json` and human output, and the daemon's own
/// `LARGEST_TABLES_LIMIT` (`server/src/handlers/status.rs`) — kept equal by
/// convention, not by a shared constant, since the two crates don't share a
/// dependency edge for it.
const LARGEST_TABLES_LIMIT: usize = 5;

/// Per-store figures gathered for `status`'s output — identical whether they
/// came from the embedded `RetrievalStore::stats()` call or from a daemon's
/// `GET /v1/status` response (issue #187 stage 5); `run_daemon`/`run_embedded`
/// below are the only two places that distinction still exists.
///
/// `stats` is `None` when per-store stats weren't available (e.g. a corrupt
/// or mid-migration store, embedded mode; or a store the daemon couldn't
/// stat) — `status` must keep reporting on the daemon state and the other
/// stores rather than aborting outright.
#[derive(Debug, Clone)]
pub(crate) struct StoreStatusEntry {
    pub name: String,
    pub visibility: &'static str,
    pub backend: String,
    pub stats: Option<localdb_core::StoreStats>,
}

/// The mode-agnostic result of `localdb status` — one shared renderer
/// (`build_status_json`/`print_status_human`) consumes this regardless of
/// which transport produced it.
pub(crate) struct StatusOutcome {
    pub(crate) daemon_status: String,
    pub(crate) stores: Vec<StoreStatusEntry>,
    pub(crate) db_path: PathBuf,
    pub(crate) db_size: DbFileSize,
    pub(crate) largest_tables: Vec<localdb_core::TableSize>,
}

/// Sum of `chunk_count` across every store whose stats were available.
fn total_chunk_count(stores: &[StoreStatusEntry]) -> u64 {
    stores
        .iter()
        .filter_map(|s| s.stats.as_ref())
        .map(|s| s.chunk_count)
        .sum()
}

/// Coerce an arbitrary visibility string (from a daemon's JSON response) to
/// the `&'static str` `StoreStatusEntry::visibility` expects. Anything other
/// than "shared" defaults to "private" — the same default embedded mode's
/// own `visibility_to_string` never needs to fall back from, since it's
/// driven off a typed `StoreVisibility` enum rather than a wire string.
fn static_visibility(s: &str) -> &'static str {
    match s {
        "shared" => "shared",
        _ => "private",
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

/// `localdb status`, table-driven (issue #187 stage 5): `status` used to
/// query only a display string from the daemon and pull every count from the
/// local DB regardless of which mode was active (specs/05-surfaces.md's
/// "queries daemon" claim was false — see issue #187 §2). `run_daemon` below
/// is the daemon's first real contribution to `status`'s numbers.
pub(crate) struct StatusCmd;

impl DaemonAwareCommand for StatusCmd {
    type Outcome = StatusOutcome;

    // specs/05-surfaces.md §2.2: `--store` is repeatable and always validated
    // and resolved; the "all stores" behavior only applies when `-s` is
    // omitted, and a store-less database is exit 2 like every other
    // all-stores command.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<StatusOutcome, Error> {
        // Scope the request itself with a repeatable, percent-encoded
        // `?store=` per requested name (issue #187 review, finding F7) —
        // previously this always fetched every store and relied entirely on
        // `apply_daemon_store_scope` below to filter client-side, which both
        // wasted work gathering stores the caller didn't ask about and gave
        // an unscoped daemon-side view of `store_count`/`source_count`.
        // `encode_path_segment` is safe in a query-value position too (see
        // its own doc comment; it's already used this way for `?cursor=` in
        // `walk_daemon_pages`).
        //
        // H2 (Codex review, PR #212): validate every requested name for
        // traversal-safety *before* the request is built at all — mirrors
        // the in-`run_daemon` loop idiom `source remove` uses
        // (`cli/src/cmds/source.rs`'s `SourceRemoveCmd::run_daemon`).
        // Previously this validation only happened after the response came
        // back, via `apply_daemon_store_scope` below — so `../bad` reached
        // the daemon first and its exit code depended on daemon
        // reachability (404/exit 3 if reachable, DaemonUnreachable/exit 5
        // otherwise) instead of the stable exit 2 every other command gives
        // an unsafe name.
        for name in &ctx.stores {
            crate::normalize::validate_store_name(name)?;
        }
        let url = if ctx.stores.is_empty() {
            format!("{base_url}/v1/status")
        } else {
            let query: String = ctx
                .stores
                .iter()
                .map(|name| format!("store={}", encode_path_segment(name)))
                .collect::<Vec<_>>()
                .join("&");
            format!("{base_url}/v1/status?{query}")
        };
        let v = daemon_request_async(reqwest::Method::GET, &url, None).await?;

        let daemon_stores: Vec<StoreStatusEntry> = v
            .get("stores")
            .and_then(|s| s.as_array())
            .ok_or_else(|| Error::Internal {
                message: "daemon status response missing 'stores' array".to_string(),
                correlation_id: "daemon_status_shape".to_string(),
            })?
            .iter()
            .map(|s| StoreStatusEntry {
                name: s
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string(),
                visibility: static_visibility(
                    s.get("visibility").and_then(|n| n.as_str()).unwrap_or(""),
                ),
                backend: s
                    .get("backend")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string(),
                stats: match (
                    s.get("document_count").and_then(|n| n.as_u64()),
                    s.get("chunk_count").and_then(|n| n.as_u64()),
                ) {
                    (Some(document_count), Some(chunk_count)) => Some(localdb_core::StoreStats {
                        document_count,
                        chunk_count,
                    }),
                    _ => None,
                },
            })
            .collect();

        // The daemon reports every store it knows about; `--store` filters
        // that list client-side through the exact same policy-application
        // logic `resolve_daemon_store_scope` uses (`apply_daemon_store_scope`)
        // rather than a second, potentially-drifting implementation.
        let stores =
            apply_daemon_store_scope(&daemon_stores, |s| s.name.as_str(), ctx, Self::SCOPE_POLICY)?;

        let db = v.get("database").ok_or_else(|| Error::Internal {
            message: "daemon status response missing 'database' object".to_string(),
            correlation_id: "daemon_status_shape".to_string(),
        })?;
        let db_path = PathBuf::from(db.get("path").and_then(|p| p.as_str()).unwrap_or(""));
        let db_size = DbFileSize {
            main_bytes: db.get("size_bytes").and_then(|n| n.as_u64()),
            wal_bytes: db.get("wal_size_bytes").and_then(|n| n.as_u64()),
        };
        let largest_tables: Vec<localdb_core::TableSize> = db
            .get("largest_tables")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| Error::Internal {
                message: format!("cannot parse daemon status 'largest_tables': {e}"),
                correlation_id: "daemon_status_shape".to_string(),
            })?
            .unwrap_or_default();

        Ok(StatusOutcome {
            daemon_status: format!("running ({base_url})"),
            stores,
            db_path,
            db_size,
            largest_tables,
        })
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<StatusOutcome, Error> {
        let db_path = config_loader.paths.db_path();
        let runtime_stores = resolve_store_scope_inner(ctx, db, Self::SCOPE_POLICY).await?;
        let stores = gather_store_status(db, &runtime_stores).await;

        let db_size = compute_db_file_size(&db_path);
        // Best-effort diagnostic (see `StoreBackend::largest_tables`'s doc
        // comment) — an error here must not take `status` down with it.
        let largest_tables = db
            .backend()
            .largest_tables(LARGEST_TABLES_LIMIT)
            .await
            .unwrap_or_default();

        Ok(StatusOutcome {
            daemon_status: "not running (embedded mode)".to_string(),
            stores,
            db_path,
            db_size,
            largest_tables,
        })
    }
}

/// `localdb status`
pub fn run_status(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_status_async(ctx));
}

pub(crate) async fn run_status_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so status works even with malformed config.
    let config_loader = load_config_lenient(ctx).await;

    let outcome = dispatch(&StatusCmd, ctx, &config_loader, || {
        open_app_db_lenient_or_exit(ctx, &config_loader)
    })
    .await;

    if ctx.json {
        print_json(&build_status_json(
            &outcome.daemon_status,
            &outcome.stores,
            &outcome.db_path,
            outcome.db_size,
            &outcome.largest_tables,
        ));
    } else {
        print_status_human(
            &outcome.daemon_status,
            &outcome.stores,
            &outcome.db_path,
            outcome.db_size,
            &outcome.largest_tables,
        );
    }
}

#[cfg(test)]
mod tests {
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
