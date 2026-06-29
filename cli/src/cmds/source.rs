use localdb_core::{
    ids::new_ulid, ingestion::now_rfc3339, source::normalize_path_source, types::SourceKind, Error,
    SourceRow,
};
use serde_json::json;

use crate::{
    app_db::{load_app_db, resolve_store_name},
    cmds::index::{run_embedded_index, IndexErrorMode},
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{
        classify_source, exit_err, looks_like_id, print_json, source_kind_to_string,
        validate_store_name,
    },
};

/// `localdb source add <path-or-url>`
pub fn run_source_add(ctx: &CliContext, source_arg: &str, refresh: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_add_async(ctx, source_arg, refresh));
}

pub(crate) async fn run_source_add_async(
    ctx: &CliContext,
    source_arg: &str,
    refresh: Option<&str>,
) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    // A9-safety: validate the --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let store_name = resolve_store_name(ctx, &db).await;

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let (kind, _root, url) = classify_source(source_arg);
        // The handler's CreateSourceRequest expects {kind, spec, preset} where
        // spec is a nested object (see server/src/handlers.rs CreateSourceRequest).
        // Apply the same path normalization as embedded mode (#14, #7, #4).
        let spec = if kind == "path" {
            match normalize_path_source(source_arg) {
                Ok((root, include, exclude)) => {
                    json!({ "root": root, "include": include, "exclude": exclude })
                }
                Err(e) => exit_err(&e, ctx.json),
            }
        } else {
            json!({ "url": url })
        };
        let url_str = format!("{}/v1/stores/{}/sources", base_url, store_name);
        let body = json!({
            "kind": kind,
            "spec": spec,
            "preset": "prose",
            "refresh": refresh,
        });
        match daemon_request_async(reqwest::Method::POST, &url_str, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!(
                        "Added source {} to store '{}' (via daemon)",
                        v.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
                        store_name
                    );
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // #13: Verify store exists in runtime DB (exit 3 if not found).
    let rt_store = match db.backend().get_store_by_name(&store_name).await {
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store_name.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
        Ok(Some(s)) => s,
    };

    let (kind, _root_str, url_str2) = classify_source(source_arg);

    // Normalize path sources: validate existence, promote single files, apply excludes.
    let (actual_root, include_globs, exclude_globs) = if kind == "path" {
        match normalize_path_source(source_arg) {
            Ok(v) => v,
            Err(e) => exit_err(&e, ctx.json),
        }
    } else {
        (source_arg.to_string(), vec![], vec![])
    };

    // Validate refresh interval before persisting.
    if let Some(r) = refresh {
        if let Err(e) = localdb_core::config::validate_refresh_interval(r) {
            exit_err(&e, ctx.json);
        }
    }

    if refresh.is_some() && kind != "url" {
        exit_err(
            &Error::InvalidRequest {
                message: "refresh is only supported for URL sources".to_string(),
            },
            ctx.json,
        );
    }

    let src = SourceRow {
        id: new_ulid(),
        store_id: rt_store.id.clone(),
        kind: match kind {
            "url" => SourceKind::Url,
            "path" => SourceKind::Path,
            _ => SourceKind::Path,
        },
        root: if kind == "path" {
            Some(actual_root)
        } else {
            None
        },
        url: url_str2.map(|s| s.to_string()),
        include: include_globs,
        exclude: exclude_globs,
        preset: "prose".to_string(),
        refresh: refresh.map(|s| s.to_string()),
        created_at: now_rfc3339(),
    };

    if let Err(e) = db.backend().upsert_source(&src).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "id": src.id,
            "store": { "name": store_name },
            "kind": source_kind_to_string(&src.kind),
        }));
    } else {
        println!("Added source {} to store '{}'", src.id, store_name);
    }

    // #2: Auto-index after source add.
    // Drop the db handle before re-entering the index path, which opens its own.
    let src_id = src.id.clone();
    let rt_store_clone = rt_store.clone();
    drop(db);
    drop(config_loader);

    if kind == "path" || kind == "url" {
        if !ctx.json {
            eprintln!("Auto-indexing source {} ...", src_id);
        }
        // Build an index context scoped to this store.
        let index_ctx = CliContext {
            config: ctx.config.clone(),
            json: ctx.json,
            stores: vec![store_name.clone()],
            yes: false,
            daemon_url: ctx.daemon_url.clone(),
            config_env: ctx.config_env.clone(),
        };
        if let Err(e) = run_embedded_index(
            &index_ctx,
            &rt_store_clone,
            Some(&src_id),
            IndexErrorMode::WarnAndContinue,
        )
        .await
        {
            exit_err(&e, ctx.json);
        }
    }
}
/// `localdb source list`
pub fn run_source_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_list_async(ctx));
}

pub(crate) async fn run_source_list_async(ctx: &CliContext) {
    let (_, db) = load_app_db(ctx).await;

    // A9-safety: validate --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let store_name = resolve_store_name(ctx, &db).await;

    // D1: verify store exists before listing sources.
    if let Some(explicit) = ctx.stores.first() {
        match db.backend().get_store_by_name(explicit).await {
            Ok(None) => exit_err(
                &Error::StoreNotFound {
                    id: explicit.clone(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
            Ok(Some(_)) => {}
        }
    }

    let store_row = match db.backend().get_store_by_name(&store_name).await {
        Ok(Some(s)) => s,
        Ok(None) => exit_err(
            &Error::StoreNotFound {
                id: store_name.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    };

    let sources = match db.backend().list_sources(&store_row.id).await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    if ctx.json {
        // D4: include store as an object matching the citation shape.
        let json_sources: Vec<serde_json::Value> = sources
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "store": { "name": store_name },
                    "store_id": s.store_id,
                    "kind": source_kind_to_string(&s.kind),
                    "root": s.root,
                    "url": s.url,
                    "preset": s.preset,
                })
            })
            .collect();
        print_json(&json!({ "sources": json_sources }));
    } else if sources.is_empty() {
        println!("No sources on store '{}'.", store_name);
    } else {
        for s in &sources {
            let loc = s.root.as_deref().or(s.url.as_deref()).unwrap_or("?");
            println!("{} [{}] {}", s.id, source_kind_to_string(&s.kind), loc);
        }
    }
}

/// `localdb source remove <id-or-path-or-url>`
pub fn run_source_remove(ctx: &CliContext, id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_remove_async(ctx, id));
}

pub(crate) async fn run_source_remove_async(ctx: &CliContext, id: &str) {
    // A9-safety: validate --store name if given explicitly.
    if let Some(store_name) = ctx.stores.first() {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    // D1: verify the store exists if --store was given explicitly.
    if let Some(explicit) = ctx.stores.first() {
        match db.backend().get_store_by_name(explicit).await {
            Ok(None) => exit_err(
                &Error::StoreNotFound {
                    id: explicit.clone(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
            Ok(Some(_)) => {}
        }
    }

    // Per specs/05-surfaces.md §2: route to daemon when running.
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        // Route is DELETE /v1/sources/{id} (see server/src/daemon.rs build_router).
        let url = format!("{}/v1/sources/{}", base_url, id);
        match daemon_request_async(reqwest::Method::DELETE, &url, None).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!("Removed source: {} (via daemon)", id);
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    // #3: Resolve the source ID. If the argument looks like a path or URL
    // (not a ULID/UUID), look it up by root/url field.
    let explicit_store = ctx.stores.first().map(|s| s.as_str());
    if !looks_like_id(id) && explicit_store.is_none() {
        exit_err(
            &Error::InvalidRequest {
                message: "source remove by path/url requires --store; pass --store <name> or use the source ULID".into(),
            },
            ctx.json,
        );
    }
    let resolved_store_id = match explicit_store {
        Some(name) => Some(match db.resolve_store_id(name).await {
            Ok(id) => id,
            Err(e) => exit_err(&e, ctx.json),
        }),
        None => None,
    };
    let resolved_id: String = if !looks_like_id(id) {
        let Some(store_id) = resolved_store_id.as_deref() else {
            exit_err(
                &Error::InvalidRequest {
                    message: "source remove by path/url requires --store; pass --store <name> or use the source ULID".into(),
                },
                ctx.json,
            );
        };
        match db.backend().find_source_by_root_or_url(id, store_id).await {
            Ok(Some(src)) => src.id,
            Ok(None) => exit_err(&Error::SourceNotFound { id: id.to_string() }, ctx.json),
            Err(e) => exit_err(&e, ctx.json),
        }
    } else {
        id.to_string()
    };

    // D2: If --store was given, verify the source belongs to that store.
    if let Some(expected_store_id) = resolved_store_id.as_deref() {
        match db.backend().get_source(&resolved_id).await {
            Ok(Some(src)) if src.store_id != expected_store_id => {
                exit_err(
                    &Error::SourceNotFound {
                        id: resolved_id.clone(),
                    },
                    ctx.json,
                );
            }
            Ok(None) => exit_err(
                &Error::SourceNotFound {
                    id: resolved_id.clone(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
            Ok(Some(_)) => {}
        }
    }

    match db.backend().delete_source(&resolved_id).await {
        Ok(true) => {}
        Ok(false) => exit_err(
            &Error::SourceNotFound {
                id: resolved_id.clone(),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "id": resolved_id }));
    } else {
        println!("Removed source: {}", resolved_id);
    }
}
