use localdb_core::{
    config::loader::{load_config, LoadOptions},
    Error,
};
use serde_json::json;

use crate::{
    app_db::load_app_db,
    daemon_client::{probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json},
};

/// `localdb serve` — start the HTTP daemon (specs/05-surfaces.md §3).
pub fn run_serve(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_serve_async(ctx));
}

pub(crate) async fn run_serve_async(ctx: &CliContext) {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options, ctx.config_env.as_deref()) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    };
    if let Err(e) = std::fs::create_dir_all(&config_loader.paths.data_dir) {
        exit_err(
            &Error::Internal {
                message: format!("cannot create data dir: {}", e),
                correlation_id: "serve_datadir".to_string(),
            },
            ctx.json,
        );
    }

    let daemon_options = server::DaemonOptions {
        paths: config_loader.paths.clone(),
        config: config_loader.config.clone(),
    };
    match server::start_daemon(daemon_options).await {
        Ok((handle, fut)) => {
            // Announce the bound address before blocking on the server future
            // so callers (and tests) can discover an OS-assigned port.
            if ctx.json {
                print_json(&json!({
                    "status": "listening",
                    "url": format!("http://{}", handle.addr),
                }));
            } else {
                println!("daemon listening on http://{}", handle.addr);
            }
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            fut.await;
            // Keep the handle (write lock + socket) alive until shutdown.
            drop(handle);
        }
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// `localdb mcp` — run the MCP server on stdio (specs/05-surfaces.md §4).
pub fn run_mcp(ctx: &CliContext, allow_write: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_mcp_async(ctx, allow_write));
}

pub(crate) async fn run_mcp_async(ctx: &CliContext, allow_write: bool) {
    use mcp::{proxy::ProxyHandler, McpHandler};

    // `load_app_db` is unconditional here — same sequencing as
    // `search.rs`'s `run_search_async` — since `probe_daemon` needs
    // `config_loader.paths.data_dir` regardless of which mode we end up in.
    // SQLite WAL mode makes opening it harmless even when a daemon is
    // already running (see `app_db::load_app_db`'s doc comment); in the
    // `Proxied` branch below, `db`/`config_loader` simply go unused beyond
    // this point, exactly as `search.rs`'s `SearchMode::Daemon` branch
    // leaves its own `db` unused.
    let (config_loader, db) = load_app_db(ctx).await;

    if let DaemonState::Running { base_url } =
        probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref())
    {
        // The daemon's `/mcp` route has no notion of a stdio caller's
        // `--store` scope (specs/05-surfaces.md §4) — re-filtering
        // client-side would mean re-deriving store visibility rules the
        // daemon already applied, for a flag combination narrow enough not
        // to be worth it in v1. Warn instead of silently ignoring it.
        if !ctx.stores.is_empty() {
            eprintln!(
                "warning: --store is not honored when a daemon is running; \
                 the daemon's full store set will be used instead"
            );
        }
        // Connect and serve are separate calls (`ProxyHandler::connect` then
        // `mcp::serve_proxied_stdio`, rather than one moded entrypoint) so a
        // failure to reach the daemon at all — it went away between
        // `probe_daemon` and here, or `LOCALDB_DAEMON_URL` points at a stale
        // endpoint — maps to the same `daemon_unreachable`/exit-5 outcome as
        // every other daemon-backed CLI path, instead of `internal`/exit-1.
        // Only a failure in the stdio loop *after* a successful proxy
        // connection (a much rarer case) still falls back to `internal`.
        let handler = match ProxyHandler::connect(&base_url).await {
            Ok(handler) => handler,
            Err(_) => {
                exit_err(&Error::DaemonUnreachable, ctx.json);
            }
        };
        if let Err(e) = mcp::serve_proxied_stdio(handler).await {
            exit_err(
                &Error::Internal {
                    message: format!("mcp stdio loop failed: {}", e),
                    correlation_id: "mcp_stdio".to_string(),
                },
                ctx.json,
            );
        }
        return;
    }

    let embed_policy = &config_loader.config.defaults.indexing.embedding;
    let models_dir = config_loader.paths.models_dir.clone();
    let embedder = match embed::create_embedder(
        embed_policy,
        &config_loader.config.providers,
        Some(&models_dir),
    ) {
        Ok(e) => e,
        Err(e) => exit_err(&Error::from(e), ctx.json),
    };

    // Realtime store resolution (T2): rather than snapshotting the runtime
    // store list once here, `AppDbStoreProvider` re-derives it from the DB
    // on every tool call, narrowed by `--store` flags when given (empty
    // means "all runtime stores, whatever they are at call time"). A store
    // added later (e.g. by a concurrent `localdb store add`, or another
    // process sharing the same WAL-mode database) is therefore visible
    // without restarting this stdio process.
    let provider: std::sync::Arc<dyn mcp::StoreProvider> = std::sync::Arc::new(
        crate::app_db::AppDbStoreProvider::new(std::sync::Arc::new(db), ctx.stores.clone()),
    );

    let handler = McpHandler::new(provider, std::sync::Arc::from(embedder), allow_write);

    if let Err(e) = mcp::serve_embedded_stdio(handler).await {
        exit_err(
            &Error::Internal {
                message: format!("mcp stdio loop failed: {}", e),
                correlation_id: "mcp_stdio".to_string(),
            },
            ctx.json,
        );
    }
}
