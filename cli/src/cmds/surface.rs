use localdb_core::{
    config::loader::{load_config, LoadOptions},
    Error,
};
use serde_json::json;

use crate::{
    app_db::load_app_db,
    daemon_client::CliContext,
    normalize::{exit_err, print_json, visibility_to_string},
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
    use mcp::{AvailableStore, McpServer, StoreDescriptor};

    let (config_loader, db) = load_app_db(ctx).await;

    let runtime_stores = match db.backend().list_stores().await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Same store resolution as `localdb search`: runtime stores only,
    // narrowed by --store flags when given.
    let store_names: Vec<String> = if ctx.stores.is_empty() {
        runtime_stores.iter().map(|s| s.name.clone()).collect()
    } else {
        ctx.stores.clone()
    };

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

    let mut available: Vec<AvailableStore> = Vec::new();
    for name in &store_names {
        if let Some(store_row) = runtime_stores.iter().find(|s| s.name == *name) {
            let descriptor = StoreDescriptor {
                id: store_row.id.clone(),
                name: store_row.name.clone(),
                visibility: visibility_to_string(&store_row.visibility).to_string(),
            };
            let handle = match db.backend().retrieval_store(&store_row.id).await {
                Ok(handle) => handle,
                Err(e) => exit_err(&e, ctx.json),
            };
            available.push(AvailableStore::from_arc(descriptor, handle));
        }
    }

    let mut mcp_server = McpServer::new(available, embedder);
    mcp_server.allow_write = allow_write;

    if let Err(e) = mcp::run_stdio_loop(&mcp_server).await {
        exit_err(
            &Error::Internal {
                message: format!("mcp stdio loop failed: {}", e),
                correlation_id: "mcp_stdio".to_string(),
            },
            ctx.json,
        );
    }
}
