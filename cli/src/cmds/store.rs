use localdb_core::Error;
use serde_json::json;

use crate::{
    app_db::{default_store_row, load_app_db, load_app_db_lenient},
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{
        confirm_destructive, exit_err, print_json, validate_store_name, visibility_to_string,
    },
};

/// `localdb store add <name>`
pub fn run_store_add(ctx: &CliContext, name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_add_async(ctx, name));
}

pub(crate) async fn run_store_add_async(ctx: &CliContext, name: &str) {
    // A9-safety: validate store name before anything else.
    if let Err(e) = validate_store_name(name) {
        exit_err(&e, ctx.json);
    }

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{}/v1/stores", base_url);
        let body = json!({ "name": name, "visibility": "private", "backend": "libsql" });
        match daemon_request_async(ctx, reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!(
                        "Added store: {} (via daemon)",
                        v.get("name").and_then(|n| n.as_str()).unwrap_or(name)
                    );
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // Duplicate check.
    match db.backend().get_store_by_name(name).await {
        Ok(Some(_)) => exit_err(
            &Error::InvalidRequest {
                message: format!("store '{}' already exists", name),
            },
            ctx.json,
        ),
        Ok(None) => {}
        Err(e) => exit_err(&e, ctx.json),
    }

    let store = match default_store_row(name, &db) {
        Ok(store) => store,
        Err(e) => exit_err(&e, ctx.json),
    };
    if let Err(e) = db.backend().upsert_store(&store).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "name": name, "id": store.id }));
    } else {
        println!("Added store: {}", name);
    }
}

/// `localdb store list`
pub fn run_store_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_list_async(ctx));
}

pub(crate) async fn run_store_list_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so store list works even with malformed config.
    let (_, db) = load_app_db_lenient(ctx).await;

    let runtime_stores = match db.backend().list_stores().await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    let all: Vec<serde_json::Value> = runtime_stores
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
        print_json(&json!({ "stores": all }));
    } else if all.is_empty() {
        println!("No stores.");
    } else {
        for s in &all {
            println!(
                "{} [{}]",
                s["name"].as_str().unwrap_or("?"),
                s["backend"].as_str().unwrap_or("?"),
            );
        }
    }
}

/// `localdb store remove <name>`
pub fn run_store_remove(ctx: &CliContext, name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_remove_async(ctx, name));
}

pub(crate) async fn run_store_remove_async(ctx: &CliContext, name: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    let prompt = format!(
        "This permanently deletes store '{}', its sources, and its index data. Continue?",
        name
    );
    if !confirm_destructive(ctx, &prompt) {
        return;
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{}/v1/stores/{}", base_url, name);
        match daemon_request_async(ctx, reqwest::Method::DELETE, &url, None).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!("Removed store: {} (via daemon)", name);
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let store_id = match db.resolve_store_id(name).await {
        Ok(id) => id,
        Err(e) => exit_err(&e, ctx.json),
    };
    match db.backend().delete_store(&store_id).await {
        Ok(true) => {}
        Ok(false) => exit_err(
            &Error::StoreNotFound {
                id: name.to_string(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "name": name }));
    } else {
        println!("Removed store: {}", name);
    }
}

// ---------------------------------------------------------------------------
// T5: `store grant|revoke` — daemon-routed with direct-DB fallback, admin
// only when daemon-routed (specs/05-surfaces.md §2, §3.1 D7).
// ---------------------------------------------------------------------------

/// Resolve a user identifier that may be a name or an ID, for the
/// direct-DB path (mirrors `server::handlers::grants::resolve_user`).
async fn resolve_user_direct_db(
    db: &crate::app_db::AppDb,
    ctx: &CliContext,
    ident: &str,
) -> localdb_core::auth::UserRow {
    use localdb_core::auth::AuthStore as _;
    if let Ok(Some(u)) = db.auth_store().get_user_by_name(ident).await {
        return u;
    }
    match db.auth_store().get_user(ident).await {
        Ok(Some(u)) => u,
        Ok(None) => exit_err(
            &Error::InvalidRequest {
                message: format!("no user named or with id '{ident}'"),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// `localdb store grant <store> <user>`
pub fn run_store_grant(ctx: &CliContext, store: &str, user: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_grant_async(ctx, store, user));
}

pub(crate) async fn run_store_grant_async(ctx: &CliContext, store: &str, user: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/stores/{store}/grants");
        let body = json!({ "user": user });
        match daemon_request_async(ctx, reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!("Granted '{user}' access to store '{store}' (via daemon)");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let store_row = match db.backend().get_store_by_name(store).await {
        Ok(Some(s)) => s,
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store.to_string(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    };
    let target_user = resolve_user_direct_db(&db, ctx, user).await;
    if let Err(e) = db
        .auth_service()
        .grant_store(store, store_row.visibility, &target_user.id, "local")
        .await
    {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "store": store, "user": target_user.name }));
    } else {
        println!("Granted '{}' access to store '{}'", target_user.name, store);
    }
}

/// `localdb store revoke <store> <user>`
pub fn run_store_revoke(ctx: &CliContext, store: &str, user: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_revoke_async(ctx, store, user));
}

pub(crate) async fn run_store_revoke_async(ctx: &CliContext, store: &str, user: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/stores/{store}/grants/{user}");
        match daemon_request_async(ctx, reqwest::Method::DELETE, &url, None).await {
            Ok(_) => {
                if ctx.json {
                    print_json(&json!({ "status": "ok", "store": store, "user": user }));
                } else {
                    println!("Revoked '{user}' access to store '{store}' (via daemon)");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let target_user = resolve_user_direct_db(&db, ctx, user).await;
    match db.auth_service().revoke_store(store, &target_user.id).await {
        Ok(true) => {}
        Ok(false) => exit_err(
            &Error::InvalidRequest {
                message: format!("no grant for user '{user}' on store '{store}'"),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "store": store, "user": target_user.name }));
    } else {
        println!("Revoked '{}' access to store '{}'", target_user.name, store);
    }
}
