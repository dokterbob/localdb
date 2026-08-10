use serde_json::json;

use crate::{
    app_db::{load_app_db_lenient, resolve_store_scope, StoreScopePolicy},
    daemon_client::{probe_daemon, CliContext, DaemonState},
    normalize::{print_json, visibility_to_string},
};

/// `localdb status`
pub fn run_status(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_status_async(ctx));
}

pub(crate) async fn run_status_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so status works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    let daemon_status = match probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        DaemonState::Running { base_url } => format!("running ({})", base_url),
        DaemonState::NotRunning => "not running (embedded mode)".to_string(),
    };

    // specs/05-surfaces.md §2.2: `--store` is repeatable and always validated
    // and resolved; the "all stores" behavior only applies when `-s` is
    // omitted. Route through the shared resolver rather than listing
    // everything unconditionally.
    let runtime_stores = resolve_store_scope(ctx, &db, StoreScopePolicy::AllStores).await;

    let all_stores: Vec<serde_json::Value> = runtime_stores
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "visibility": visibility_to_string(&s.visibility),
                "backend": s.backend,
            })
        })
        .collect();

    if ctx.json {
        print_json(&json!({
            "daemon": daemon_status,
            "stores": all_stores,
        }));
    } else {
        println!("daemon: {}", daemon_status);
        println!("stores ({}):", all_stores.len());
        if all_stores.is_empty() {
            println!("  (none)");
        }
        for s in &all_stores {
            println!(
                "  {} [{}]",
                s["name"].as_str().unwrap_or("?"),
                s["backend"].as_str().unwrap_or("?"),
            );
        }
    }
}
