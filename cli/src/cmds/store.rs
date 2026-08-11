use localdb_core::Error;
use serde_json::json;

use crate::{
    app_db::{
        default_store_row, load_app_db, load_app_db_lenient, reject_store_flag,
        resolve_store_scope, StoreScopePolicy, STORE_ADD_REJECT_MESSAGE,
        STORE_REMOVE_REJECT_MESSAGE,
    },
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
    // specs/05-surfaces.md §2.2: the store this command acts on is its
    // positional argument, so `-s` has nothing to select. It used to be
    // silently ignored — `localdb -s books store add x` created `x`, not
    // anything to do with `books` — which is the #178 failure mode (#201).
    reject_store_flag(ctx, STORE_ADD_REJECT_MESSAGE);

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
        match daemon_request_async(reqwest::Method::POST, &url, Some(body)).await {
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

    // specs/05-surfaces.md §2.2: `--store` is repeatable and always validated
    // and resolved; the "all stores" behavior only applies when `-s` is
    // omitted. Route through the shared resolver rather than listing
    // everything unconditionally.
    let runtime_stores = resolve_store_scope(ctx, &db, StoreScopePolicy::AllStores).await;

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
    // specs/05-surfaces.md §2.2, as for `store add` above. First statement in
    // the function on purpose: it must precede `confirm_destructive` below,
    // so a misused flag never gets as far as asking the user to confirm a
    // deletion this invocation was never going to perform correctly.
    reject_store_flag(ctx, STORE_REMOVE_REJECT_MESSAGE);

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
        // `name` is percent-encoded before it's interpolated into the URL
        // path segment — see `daemon_client::encode_path_segment`'s doc
        // comment (finding 1): an unescaped '#'/'?'/'/' would otherwise
        // retarget the DELETE at a different daemon endpoint entirely.
        let url = format!(
            "{}/v1/stores/{}",
            base_url,
            crate::daemon_client::encode_path_segment(name)
        );
        match daemon_request_async(reqwest::Method::DELETE, &url, None).await {
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
