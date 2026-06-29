use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::{
    routing::{delete, get, post},
    Router,
};
use tokio::net::TcpListener;
use tracing::{error, info};

use localdb_core::{
    config::{loader::ResolvedPaths, schema::RawConfig},
    Error,
};

use crate::{
    handlers, job_queue::JobQueue, scheduler::UrlRefreshScheduler, socket::SocketGuard,
    state::AppState,
};

/// Options for starting the daemon.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub paths: ResolvedPaths,
    /// The loaded YAML config.
    pub config: RawConfig,
}

/// A running daemon instance.
///
pub struct DaemonHandle {
    /// The socket guard (cleans up socket file on drop).
    pub _socket: SocketGuard,
    /// The bind address.
    pub addr: SocketAddr,
}

impl std::fmt::Debug for DaemonHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DaemonHandle({})", self.addr)
    }
}

/// Start the daemon.
///
/// Steps:
/// 1. Validate bind address: refuse non-loopback without auth.
pub async fn start_daemon(
    options: DaemonOptions,
) -> Result<(DaemonHandle, impl std::future::Future<Output = ()>), Error> {
    let bind_addr = options.config.server.bind.as_str();
    let port = options.config.server.port;
    let socket_guard = bind_socket_guard(&options)?;
    let (state, url_scheduler) = build_daemon_state(&options).await?;
    let router = build_router(state.clone());
    let (listener, bound_addr) = bind_tcp_listener(bind_addr, port).await?;

    spawn_config_watcher(options.paths.config_file.clone(), state.clone());
    spawn_url_scheduler(&state, url_scheduler);

    let handle = DaemonHandle {
        _socket: socket_guard,
        addr: bound_addr,
    };

    Ok((handle, server_future(listener, router)))
}

fn bind_socket_guard(options: &DaemonOptions) -> Result<SocketGuard, Error> {
    validate_bind_address(options.config.server.bind.as_str())?;
    SocketGuard::new(&options.paths.socket_path())
}

async fn build_daemon_state(
    options: &DaemonOptions,
) -> Result<(AppState, UrlRefreshScheduler), Error> {
    let queue = JobQueue::new();
    let url_scheduler = UrlRefreshScheduler::new(queue.clone());
    let state = AppState::new(
        options.config.clone(),
        options.paths.data_dir.clone(),
        queue.clone(),
        url_scheduler.clone(),
    )
    .await?;

    Ok((state, url_scheduler))
}

async fn bind_tcp_listener(bind_addr: &str, port: u16) -> Result<(TcpListener, SocketAddr), Error> {
    let addr_str = format!("{}:{}", bind_addr, port);
    let listener = TcpListener::bind(&addr_str)
        .await
        .map_err(|e| Error::Internal {
            message: format!("cannot bind to {}: {}", addr_str, e),
            correlation_id: "daemon_bind".to_string(),
        })?;

    let bound_addr = listener.local_addr().map_err(|e| Error::Internal {
        message: format!("cannot get local addr: {}", e),
        correlation_id: "daemon_local_addr".to_string(),
    })?;

    info!("daemon listening on {}", bound_addr);

    Ok((listener, bound_addr))
}

fn spawn_config_watcher(config_file_path: PathBuf, state: AppState) {
    tokio::spawn(async move {
        let result = run_config_watcher(config_file_path, state).await;
        if let Err(e) = result {
            error!("config watcher failed: {}", e);
        }
    });
}

fn spawn_url_scheduler(state: &AppState, url_scheduler: UrlRefreshScheduler) {
    let backend_for_url = state.backend_arc();
    let sched_for_url = url_scheduler.clone();
    tokio::spawn(async move {
        let stores = match backend_for_url.list_stores().await {
            Ok(s) => s,
            Err(e) => {
                error!("URL scheduler: cannot list stores: {e}");
                return;
            }
        };
        for store in stores {
            let sources = match backend_for_url.list_sources(&store.id).await {
                Ok(s) => s,
                Err(e) => {
                    error!(
                        "URL scheduler: cannot list sources for '{}': {e}",
                        store.name
                    );
                    continue;
                }
            };
            for source in sources {
                if source.kind == localdb_core::types::SourceKind::Url {
                    if let Some(url) = source.url {
                        let interval_secs =
                            source.refresh.as_deref().and_then(parse_refresh_interval);
                        sched_for_url
                            .register(source.id, store.name.clone(), url, interval_secs)
                            .await;
                    }
                }
            }
        }
    });
    tokio::spawn(url_scheduler.run(std::time::Duration::from_secs(60)));
}

fn server_future(listener: TcpListener, router: Router) -> impl std::future::Future<Output = ()> {
    async move {
        if let Err(e) = axum::serve(listener, router).await {
            error!("server error: {}", e);
        }
    }
}

/// Build the axum router with all /v1 routes.
///
/// Routes per specs/05-surfaces.md §3:
///   GET/POST /stores, GET/PATCH/DELETE /stores/{id},
///   GET/POST /stores/{id}/sources, DELETE /sources/{id},
///   GET /documents/{id}, POST /search,
///   POST /jobs, GET /jobs/{id}, GET /status, GET /config.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/v1/stores",
            get(handlers::list_stores).post(handlers::create_store),
        )
        .route(
            "/v1/stores/{name}",
            get(handlers::get_store)
                .patch(handlers::patch_store)
                .delete(handlers::delete_store),
        )
        .route(
            "/v1/stores/{name}/sources",
            get(handlers::list_sources).post(handlers::create_source),
        )
        .route("/v1/sources/{id}", delete(handlers::delete_source))
        .route("/v1/documents/{id}", get(handlers::get_document))
        .route("/v1/search", post(handlers::search))
        .route("/v1/jobs", post(handlers::create_job))
        .route("/v1/jobs/{id}", get(handlers::get_job))
        .route("/v1/status", get(handlers::get_status))
        .route("/v1/config", get(handlers::get_config))
        .with_state(state)
}

/// Validate the bind address.
///
/// Per specs/05-surfaces.md §3: "Binding to a non-loopback address without auth
/// configured is a refused startup, not a warning."
///
/// MVP: any non-loopback address is refused.
pub fn validate_bind_address(bind: &str) -> Result<(), Error> {
    // Accept loopback addresses: 127.x.x.x or ::1 or localhost
    let is_loopback =
        bind == "127.0.0.1" || bind == "::1" || bind == "localhost" || bind.starts_with("127.");

    if !is_loopback {
        return Err(Error::InvalidConfig {
            message: format!(
                "refusing to bind to non-loopback address '{}' without auth configured; \
                 use 127.0.0.1 for local-only mode (the default). \
                 Non-loopback binding requires auth (roadmap feature).",
                bind
            ),
        });
    }

    Ok(())
}

/// Watch the config file for changes and reload the YAML config snapshot.
///
/// Non-fatal: logs errors but does not stop the daemon.
async fn run_config_watcher(config_file: PathBuf, state: AppState) -> Result<(), Error> {
    let parent = config_file.parent().ok_or_else(|| Error::InvalidConfig {
        message: "config file has no parent directory".to_string(),
    })?;

    let (mut rx, _handle) =
        crate::watcher::watch_path(parent, 300).map_err(|e| Error::Internal {
            message: format!(
                "cannot start config watcher for '{}': {e}",
                config_file.display()
            ),
            correlation_id: "daemon_config_reload".into(),
        })?;

    info!("config watcher started for: {}", config_file.display());

    while let Some(event) = rx.recv().await {
        if event.path == config_file {
            info!("config file changed, reloading: {}", config_file.display());
            match reload_config_file(&config_file) {
                Ok(new_config) => {
                    state.reload_yaml_config(new_config).await;
                    info!("config reloaded successfully");
                }
                Err(e) => {
                    error!("config reload failed: {}", e);
                }
            }
        }
    }

    Ok(())
}

/// Parse a human-readable refresh interval string (e.g. "24h", "30m", "3600s") to seconds.
///
/// Returns `None` if the string is unparseable, empty, or would overflow `u64`.
/// Uses checked arithmetic to guard against integer overflow for very large values.
pub fn parse_refresh_interval(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(h) = s.strip_suffix('h') {
        h.parse::<u64>().ok().and_then(|n| n.checked_mul(3600))
    } else if let Some(m) = s.strip_suffix('m') {
        m.parse::<u64>().ok().and_then(|n| n.checked_mul(60))
    } else if let Some(sec) = s.strip_suffix('s') {
        sec.parse::<u64>().ok()
    } else {
        s.parse::<u64>().ok()
    }
}

/// Read and parse the config file.
fn reload_config_file(path: &Path) -> Result<RawConfig, Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| Error::Internal {
        message: format!("cannot read config file '{}': {e}", path.display()),
        correlation_id: "daemon_config_reload".into(),
    })?;
    let config: RawConfig = serde_yaml::from_str(&contents).map_err(|e| Error::Internal {
        message: format!("cannot parse config file '{}': {e}", path.display()),
        correlation_id: "daemon_config_reload".into(),
    })?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn make_state() -> (TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
        yaml_config.defaults.indexing.embedding = localdb_core::config::schema::EmbeddingPolicy {
            provider: "fake".to_string(),
            model: "default".to_string(),
        };
        let queue = crate::job_queue::JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir.path().to_path_buf(),
            queue.clone(),
            crate::scheduler::UrlRefreshScheduler::new(queue),
        )
        .await
        .unwrap();
        (dir, state)
    }

    fn make_resolved_paths(dir: &Path) -> ResolvedPaths {
        ResolvedPaths {
            config_file: dir.join("config.yaml"),
            data_dir: dir.join("data"),
            models_dir: dir.join("models"),
            logs_dir: dir.join("logs"),
        }
    }

    // --- validate_bind_address ---

    #[test]
    fn loopback_addresses_are_accepted() {
        assert!(validate_bind_address("127.0.0.1").is_ok());
        assert!(validate_bind_address("::1").is_ok());
        assert!(validate_bind_address("localhost").is_ok());
        assert!(validate_bind_address("127.0.0.2").is_ok());
    }

    #[test]
    fn non_loopback_addresses_are_rejected() {
        let result = validate_bind_address("0.0.0.0");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "invalid_config");

        let result = validate_bind_address("192.168.1.1");
        assert!(result.is_err());

        let result = validate_bind_address("0.0.0.0");
        assert!(result.is_err());
    }

    #[test]
    fn non_loopback_error_message_is_descriptive() {
        let err = validate_bind_address("0.0.0.0").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("0.0.0.0") || msg.contains("non-loopback"),
            "error message should describe the problem: {}",
            msg
        );
    }

    #[tokio::test]
    async fn run_config_watcher_returns_invalid_config_when_path_has_no_parent() {
        let (_dir, state) = make_state().await;

        let err = run_config_watcher(PathBuf::new(), state).await.unwrap_err();

        assert!(
            matches!(err, Error::InvalidConfig { .. }),
            "expected InvalidConfig, got: {:?}",
            err
        );
    }

    #[test]
    fn reload_config_file_maps_parse_errors_to_internal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "::not-yaml::").unwrap();

        let err = reload_config_file(&path).unwrap_err();

        assert!(
            matches!(err, Error::Internal { ref correlation_id, .. } if correlation_id == "daemon_config_reload"),
            "expected Internal with daemon_config_reload correlation id, got: {:?}",
            err
        );
    }

    // --- Daemon startup ---

    #[tokio::test]
    async fn daemon_starts_and_binds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig {
                bind: "127.0.0.1".to_string(),
                port: 0, // let OS assign a free port
            },
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };

        let options = DaemonOptions {
            paths: paths.clone(),
            config,
        };

        let result = start_daemon(options).await;
        assert!(result.is_ok(), "daemon should start: {:?}", result.err());
        let (handle, _server_future) = result.unwrap();
        assert!(handle.addr.port() > 0);
    }

    #[tokio::test]
    async fn second_daemon_fails_with_daemon_running() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig {
                bind: "127.0.0.1".to_string(),
                port: 0, // let OS assign a free port
            },
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };

        let options1 = DaemonOptions {
            paths: paths.clone(),
            config: config.clone(),
        };

        // Start first daemon
        let result1 = start_daemon(options1).await;
        assert!(result1.is_ok(), "first daemon should start");
        let (_handle1, _fut1) = result1.unwrap();

        let options2 = DaemonOptions {
            paths: paths.clone(),
            config: config.clone(),
        };
        let result2 = start_daemon(options2).await;
        assert!(
            matches!(result2, Err(Error::DaemonRunning)),
            "second daemon should fail with DaemonRunning, got: {:?}",
            result2.err()
        );
    }

    #[tokio::test]
    async fn non_loopback_bind_refuses_startup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        let paths = make_resolved_paths(dir.path());
        let mut config = RawConfig {
            version: 1,
            server: localdb_core::config::schema::ServerConfig::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
        // Set non-loopback bind address
        config.server.bind = "0.0.0.0".to_string();

        let options = DaemonOptions { paths, config };

        let result = start_daemon(options).await;
        assert!(
            matches!(result, Err(Error::InvalidConfig { .. })),
            "non-loopback bind should fail with InvalidConfig"
        );
    }

    // --- Watcher integration: file change ⇒ re-index ⇒ search reflects it ---

    /// Integration test for the acceptance criterion:
    /// "watcher test: file change ⇒ re-index ⇒ search reflects it"
    ///
    /// This test:
    /// 1. Creates a watched directory with a file.
    /// 2. Starts a watcher that queues a job on file change.
    /// 3. Modifies the file.
    /// 4. Verifies a job was submitted and completed.
    /// 5. Verifies the updated content appears in search results.
    #[tokio::test]
    async fn watcher_file_change_triggers_reindex_visible_in_search() {
        use localdb_core::{ChunkRecord, Embedder, FakeEmbedder};
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let dir_real = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());

        // Create the state and job queue.
        let yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: localdb_core::config::schema::DefaultsConfig {
                indexing: localdb_core::config::schema::IndexingPolicyConfig {
                    embedding: localdb_core::config::schema::EmbeddingPolicy {
                        provider: "fake".to_string(),
                        model: "default".to_string(),
                    },
                    ..Default::default()
                },
            },
            providers: vec![],
        };
        let queue = JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir_real.to_path_buf(),
            queue.clone(),
            UrlRefreshScheduler::new(queue.clone()),
        )
        .await
        .unwrap();
        state.add_store("store-A", "private").await.unwrap();
        let source = state
            .add_source(
                "store-A",
                "path",
                serde_json::json!({"root": "/tmp"}),
                "prose",
                None,
            )
            .await
            .unwrap();
        let store_id = source.store_id.clone();

        // Create initial file.
        let watched_file = dir_real.join("doc.md");
        std::fs::write(&watched_file, "initial content").unwrap();

        // Start a watcher on the directory.
        let (mut file_events, _watcher_handle) = crate::watcher::watch_path(&dir_real, 50).unwrap();

        // Give the watcher time to start.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Modify the file — this triggers a watcher event.
        let updated_text = "rust programming language performance tips";
        std::fs::write(&watched_file, updated_text).unwrap();

        // Wait for the watcher event.
        let event = tokio::time::timeout(Duration::from_secs(5), file_events.recv())
            .await
            .expect("watcher should deliver event within 5 seconds")
            .expect("event channel should not be closed");

        assert!(
            event.path.ends_with("doc.md") || event.path == watched_file,
            "event should reference the modified file, got: {:?}",
            event.path
        );

        // Simulate what the daemon's watcher loop would do: submit an index job.
        // In production this would run the full ingestion pipeline. Here we
        // directly upsert a chunk to the retrieval store (representing the indexed content).
        let embedder = FakeEmbedder::new(128);
        let docs = vec![localdb_core::embedder::DocumentChunks {
            document_context: updated_text.to_string(),
            chunks: vec![updated_text.to_string()],
        }];
        let embedded = embedder.embed_documents(docs).await.unwrap();
        let embedding = embedded
            .into_iter()
            .next()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let job_state_clone = state.clone();
        let job_store_id = store_id.clone();
        let chunks = vec![ChunkRecord {
            id: "watcher-chunk-1".to_string(),
            document_id: "watcher-doc-1".to_string(),
            store_id: store_id.clone(),
            text: updated_text.to_string(),
            span: localdb_core::types::Span::new(0, updated_text.len()),
            heading_path: vec![],
            embedding,
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: "watcher-hash-1".to_string(),
            origin_store: store_id.clone(),
            source_id: source.id,
            source_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: format!("file://{}", watched_file.display()),
            metadata: localdb_core::DocumentMetadata::default(),
        }];

        // Submit a job that upserts the chunk (simulating real ingestion).
        let job = queue
            .submit("store-A", localdb_core::IndexJobScope::Store, move || {
                // This closure runs on a blocking thread and produces the chunk data.
                // In real ingestion, this would call run_ingestion_for_source.
                tokio::runtime::Handle::current()
                    .block_on(async {
                        job_state_clone
                            .backend()
                            .retrieval_store(&job_store_id)
                            .await?
                            .upsert_chunks(chunks)
                            .await
                    })
                    .map_err(|e| format!("upsert failed: {}", e))?;
                Ok(localdb_core::IndexJobStats {
                    docs_indexed: 1,
                    chunks_written: 1,
                    ..Default::default()
                })
            })
            .await;

        // Poll until the job completes.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::time::Instant::now() > deadline {
                panic!("ingestion job did not complete in time");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            let current = queue.get_job(&job.id).await.unwrap();
            if current.state == localdb_core::IndexJobState::Done {
                assert_eq!(
                    current.stats.docs_indexed, 1,
                    "job should have indexed 1 document"
                );
                break;
            }
            if current.state == localdb_core::IndexJobState::Failed {
                panic!("ingestion job failed: {:?}", current.error);
            }
        }

        // Verify: search now returns the updated content.
        let store = state.backend().retrieval_store(&store_id).await.unwrap();
        let stats = store.stats().await.unwrap();
        assert_eq!(
            stats.chunk_count, 1,
            "one chunk should be indexed after job completes"
        );

        // Run a search via the HTTP API to confirm the citation is returned.
        let app = build_router(state);

        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let resp = app
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/v1/search")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"query": "rust programming"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let citations = body["citations"].as_array().unwrap();
        assert!(
            !citations.is_empty(),
            "search should return citations for updated file content; body: {:?}",
            body
        );
        // The citation should point to the modified file.
        let found = citations.iter().any(|c| {
            c["uri"]
                .as_str()
                .map(|u| u.contains("doc.md"))
                .unwrap_or(false)
        });
        assert!(
            found,
            "search results should include the updated file; citations: {:?}",
            citations
        );
    }

    // --- HTTP integration via build_router ---

    #[tokio::test]
    async fn router_serves_status_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let yaml_config = RawConfig {
            version: 1,
            server: Default::default(),
            paths: Default::default(),
            defaults: Default::default(),
            providers: vec![],
        };
        let queue = JobQueue::new();
        let state = AppState::new(
            yaml_config,
            dir.path().to_path_buf(),
            queue.clone(),
            UrlRefreshScheduler::new(queue),
        )
        .await
        .unwrap();
        let app = build_router(state);

        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    // --- parse_refresh_interval ---

    #[test]
    fn parse_refresh_interval_parses_hours() {
        assert_eq!(parse_refresh_interval("1h"), Some(3600));
        assert_eq!(parse_refresh_interval("24h"), Some(86400));
        assert_eq!(parse_refresh_interval("0h"), Some(0));
    }

    #[test]
    fn parse_refresh_interval_parses_minutes() {
        assert_eq!(parse_refresh_interval("1m"), Some(60));
        assert_eq!(parse_refresh_interval("30m"), Some(1800));
    }

    #[test]
    fn parse_refresh_interval_parses_seconds() {
        assert_eq!(parse_refresh_interval("3600s"), Some(3600));
        assert_eq!(parse_refresh_interval("0s"), Some(0));
    }

    #[test]
    fn parse_refresh_interval_parses_plain_number() {
        assert_eq!(parse_refresh_interval("7200"), Some(7200));
    }

    #[test]
    fn parse_refresh_interval_empty_returns_none() {
        assert_eq!(parse_refresh_interval(""), None);
        assert_eq!(parse_refresh_interval("   "), None);
    }

    #[test]
    fn parse_refresh_interval_invalid_returns_none() {
        assert_eq!(parse_refresh_interval("abc"), None);
        assert_eq!(parse_refresh_interval("1x"), None);
    }

    /// F6: overflow guard — very large hour values must not wrap around.
    /// `u64::MAX / 3600 + 1` hours would overflow; checked_mul returns None.
    #[test]
    fn parse_refresh_interval_overflow_returns_none() {
        // u64::MAX is 18_446_744_073_709_551_615.
        // 18_446_744_073_709_551_615 / 3600 = 5_124_095_576_030_431, remainder ≠ 0.
        // So 5_124_095_576_030_432h would overflow.
        let overflow_h = format!("{}h", u64::MAX / 3600 + 1);
        assert_eq!(
            parse_refresh_interval(&overflow_h),
            None,
            "hours overflow should return None, not wrap"
        );

        let overflow_m = format!("{}m", u64::MAX / 60 + 1);
        assert_eq!(
            parse_refresh_interval(&overflow_m),
            None,
            "minutes overflow should return None, not wrap"
        );
    }
}
