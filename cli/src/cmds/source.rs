use localdb_core::{
    ids::new_ulid, ingestion::now_rfc3339, source::normalize_path_source, types::SourceKind, Error,
    SourceRow, StoreRow,
};
use serde_json::json;

use crate::{
    app_db::{load_app_db, resolve_store_scope, resolve_store_scope_names, StoreScopePolicy},
    cmds::index::{run_embedded_index, IndexErrorMode},
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{classify_source, exit_err, kind_to_string, looks_like_id, print_json},
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

    // Per specs/05-surfaces.md §2: route to daemon when running. Probed before
    // store resolution because the two paths resolve scope differently — the
    // daemon owns its own store set (see `resolve_store_scope_names`).
    let daemon_state = probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref());

    let (kind, _root, url) = classify_source(source_arg);

    // Normalize path sources: validate existence, promote single files, apply
    // excludes. Store-independent, so this runs once regardless of how many
    // stores are in scope.
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

    if let DaemonState::Running { ref base_url } = daemon_state {
        // Names only — the daemon is the authority on which stores exist.
        let store_names = resolve_store_scope_names(ctx);

        // Accumulate JSON results across the loop and emit exactly one
        // top-level document afterward (finding 3): printing per-iteration,
        // as this used to, made `--store a --store b --json source add`
        // write multiple back-to-back JSON objects to stdout, which isn't
        // parseable as a single document.
        let mut json_results: Vec<serde_json::Value> = Vec::new();

        for store_name in &store_names {
            // The handler's CreateSourceRequest expects {kind, spec, preset}
            // where spec is a nested object (see server/src/handlers.rs
            // CreateSourceRequest).
            let spec = if kind == "path" {
                json!({ "root": actual_root, "include": include_globs, "exclude": exclude_globs })
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
                        json_results.push(v);
                    } else {
                        println!(
                            "Added source {} to store '{}' (via daemon)",
                            v.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
                            store_name
                        );
                    }
                }
                Err(e) => exit_err(&e, ctx.json),
            }
        }

        if ctx.json {
            if json_results.len() == 1 {
                // Single store: today's exact flat shape — the daemon's raw
                // response, passed through unchanged (specs/05-surfaces.md
                // §2.2 promises existing scripts don't break).
                print_json(&json_results[0]);
            } else {
                print_json(&json!({ "status": "ok", "results": json_results }));
            }
        }
        return;
    }

    // specs/05-surfaces.md §2.2: bare invocation -> store named "default";
    // `-s` (repeatable) always wins and is validated/resolved/deduped here.
    let rows = resolve_store_scope(ctx, &db, StoreScopePolicy::DefaultStore).await;

    // Sources that were added locally and need auto-indexing, deferred until
    // after `db`/`config_loader` are dropped (`run_embedded_index` opens its
    // own `AppDb`).
    let mut to_index: Vec<(StoreRow, String)> = Vec::new();

    // Accumulate JSON results across the loop and emit exactly one top-level
    // document afterward (finding 3) — see the daemon branch above for the
    // same restructuring and its rationale.
    let mut json_results: Vec<serde_json::Value> = Vec::new();

    for row in &rows {
        let src = SourceRow {
            id: new_ulid(),
            store_id: row.id.clone(),
            // `classify_source` only ever yields "url" or "path", but it
            // returns a `&str`, so a `match` would need an unreachable
            // wildcard arm. Two branches keep it honest and coverable.
            kind: if kind == "url" {
                SourceKind::Url
            } else {
                SourceKind::Path
            },
            root: if kind == "path" {
                Some(actual_root.clone())
            } else {
                None
            },
            url: url.map(|s| s.to_string()),
            include: include_globs.clone(),
            exclude: exclude_globs.clone(),
            preset: "prose".to_string(),
            refresh: refresh.map(|s| s.to_string()),
            created_at: now_rfc3339(),
        };

        if let Err(e) = db.backend().upsert_source(&src).await {
            exit_err(&e, ctx.json);
        }

        if ctx.json {
            json_results.push(json!({
                "id": src.id,
                "store": { "name": row.name },
                "kind": kind_to_string(&src.kind),
            }));
        } else {
            println!("Added source {} to store '{}'", src.id, row.name);
        }

        // #2: Auto-index after source add.
        if kind == "path" || kind == "url" {
            to_index.push((row.clone(), src.id.clone()));
        }
    }

    if ctx.json {
        if json_results.len() == 1 {
            // Single store: today's exact flat shape (specs/05-surfaces.md
            // §2.2 promises existing scripts don't break).
            let r = &json_results[0];
            print_json(&json!({
                "status": "ok",
                "id": r["id"],
                "store": r["store"],
                "kind": r["kind"],
            }));
        } else {
            print_json(&json!({ "status": "ok", "results": json_results }));
        }
    }

    // Drop the db handle before re-entering the index path, which opens its own.
    drop(db);
    drop(config_loader);

    for (row, src_id) in &to_index {
        if !ctx.json {
            eprintln!("Auto-indexing source {} ...", src_id);
        }
        // Build an index context scoped to this store.
        let index_ctx = CliContext {
            config: ctx.config.clone(),
            json: ctx.json,
            stores: vec![row.name.clone()],
            yes: false,
            daemon_url: ctx.daemon_url.clone(),
            config_env: ctx.config_env.clone(),
        };
        if let Err(e) = run_embedded_index(
            &index_ctx,
            row,
            Some(src_id),
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

    // specs/05-surfaces.md §2.2: bare invocation -> store named "default".
    let rows = resolve_store_scope(ctx, &db, StoreScopePolicy::DefaultStore).await;

    let mut all: Vec<(String, SourceRow)> = Vec::new();
    for row in &rows {
        let sources = match db.backend().list_sources(&row.id).await {
            Ok(s) => s,
            Err(e) => exit_err(&e, ctx.json),
        };
        for s in sources {
            all.push((row.name.clone(), s));
        }
    }

    if ctx.json {
        // D4: include store as an object matching the citation shape.
        let json_sources: Vec<serde_json::Value> = all
            .iter()
            .map(|(store_name, s)| {
                json!({
                    "id": s.id,
                    "store": { "name": store_name },
                    "store_id": s.store_id,
                    "kind": kind_to_string(&s.kind),
                    "root": s.root,
                    "url": s.url,
                    "preset": s.preset,
                })
            })
            .collect();
        print_json(&json!({ "sources": json_sources }));
        return;
    }

    if all.is_empty() {
        if rows.len() == 1 {
            println!("No sources on store '{}'.", rows[0].name);
        } else {
            println!("No sources in scope.");
        }
        return;
    }

    // Output gains a store-name column only when more than one store is in
    // scope; a single store in scope keeps the pre-existing output format
    // (specs/05-surfaces.md §2.2).
    let col_width = store_column_width(rows.iter().map(|r| r.name.as_str()));
    for (store_name, s) in &all {
        let store_col = if rows.len() > 1 {
            Some(store_name.as_str())
        } else {
            None
        };
        println!("{}", format_source_line(store_col, col_width, s));
    }
}

/// Width of the store-name column: longest name in scope plus two spaces of
/// separation before the source line begins. Only used when `>1` store is in
/// scope; callers pass `0` (ignored) otherwise.
fn store_column_width<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    names.map(str::len).max().unwrap_or(0) + 2
}

/// Format a single `source list` line, with or without a leading store-name
/// column. `col_width` is only consulted when `store_name` is `Some`.
fn format_source_line(store_name: Option<&str>, col_width: usize, src: &SourceRow) -> String {
    let loc = src.root.as_deref().or(src.url.as_deref()).unwrap_or("?");
    match store_name {
        Some(name) => format!(
            "{:<width$}{} [{}] {}",
            name,
            src.id,
            kind_to_string(&src.kind),
            loc,
            width = col_width
        ),
        None => format!("{} [{}] {}", src.id, kind_to_string(&src.kind), loc),
    }
}

/// `localdb source remove <id-or-path-or-url>`
pub fn run_source_remove(ctx: &CliContext, id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_remove_async(ctx, id));
}

pub(crate) async fn run_source_remove_async(ctx: &CliContext, id: &str) {
    let (config_loader, db) = load_app_db(ctx).await;

    // #3: If the argument looks like a path or URL (not a ULID/UUID), it must
    // be resolved against a specific store's sources, so an explicit --store
    // is required; a bare invocation must not silently fall back to the
    // implicit "default" store scope for this case (specs/05-surfaces.md
    // §2.2 still requires callers to say which store's path/url they mean).
    if !looks_like_id(id) && ctx.stores.is_empty() {
        exit_err(
            &Error::InvalidRequest {
                message: "source remove by path/url requires --store; pass --store <name> or use the source ULID".into(),
            },
            ctx.json,
        );
    }

    // Per specs/05-surfaces.md §2: route to daemon when running. The DELETE
    // route is store-agnostic (`/v1/sources/{id}`), so this fires once
    // regardless of how many stores are in scope — and, since the daemon owns
    // its own store set, it must run before any local store resolution.
    if let DaemonState::Running { base_url } =
        probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref())
    {
        // Finding 5: validate --store names for traversal-safety before the
        // DELETE fires, matching `source add`'s daemon branch above. We
        // validate directly (not via `resolve_store_scope_names`) because its
        // empty-input case returns `["default"]`, which is meaningless for
        // remove-by-ID — there's no per-store scope to inject here, only
        // syntax-checking of whatever `--store` values were actually passed.
        for name in &ctx.stores {
            if let Err(e) = crate::normalize::validate_store_name(name) {
                exit_err(&e, ctx.json);
            }
        }

        // KNOWN LIMITATION (issue #188): `DELETE /v1/sources/{id}` is
        // store-agnostic, so daemon mode has no way to enforce that the
        // source actually belongs to a store named by `--store` — embedded
        // mode does enforce this (see the `matches` resolution below, D2).
        // Fixing that needs an HTTP API change; tracked in #188, not
        // attempted here. We deliberately do NOT add a local existence check
        // for `--store` either: `LOCALDB_DAEMON_URL` may point at a daemon on
        // another host with its own data directory, so a syntactically-valid
        // but locally-unknown store name must still reach the daemon (see
        // `resolve_store_scope_names`'s doc comment in `cli/src/app_db.rs`).
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

    // specs/05-surfaces.md §2.2: bare invocation -> store named "default".
    let rows = resolve_store_scope(ctx, &db, StoreScopePolicy::DefaultStore).await;

    // Resolve (store, source_id) matches within the scoped stores.
    let matches: Vec<(StoreRow, String)> = if looks_like_id(id) {
        // A global ID is inherently single-store: fetch it once, then check
        // that the store it actually belongs to is in scope (D2).
        let src = match db.backend().get_source(id).await {
            Ok(Some(s)) => s,
            Ok(None) => exit_err(&Error::SourceNotFound { id: id.to_string() }, ctx.json),
            Err(e) => exit_err(&e, ctx.json),
        };
        match rows.iter().find(|r| r.id == src.store_id) {
            Some(row) => vec![(row.clone(), src.id)],
            None => exit_err(&Error::SourceNotFound { id: id.to_string() }, ctx.json),
        }
    } else {
        // Path/url: look it up per resolved store; a matching root/url can
        // in principle exist in more than one store in scope.
        let mut found = Vec::new();
        for row in &rows {
            match db.backend().find_source_by_root_or_url(id, &row.id).await {
                Ok(Some(src)) => found.push((row.clone(), src.id)),
                Ok(None) => {}
                Err(e) => exit_err(&e, ctx.json),
            }
        }
        if found.is_empty() {
            exit_err(&Error::SourceNotFound { id: id.to_string() }, ctx.json);
        }
        found
    };

    let mut deleted: Vec<(String, String)> = Vec::new();
    for (row, source_id) in &matches {
        match db.backend().delete_source(source_id).await {
            Ok(true) => deleted.push((row.name.clone(), source_id.clone())),
            Ok(false) => exit_err(
                &Error::SourceNotFound {
                    id: source_id.clone(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    if ctx.json {
        if deleted.len() == 1 {
            print_json(&json!({ "status": "ok", "id": deleted[0].1 }));
        } else {
            let results: Vec<serde_json::Value> = deleted
                .iter()
                .map(|(name, sid)| json!({ "id": sid, "store": { "name": name } }))
                .collect();
            print_json(&json!({ "status": "ok", "results": results }));
        }
    } else if deleted.len() == 1 {
        println!("Removed source: {}", deleted[0].1);
    } else {
        for (name, sid) in &deleted {
            println!("Removed source: {} from store '{}'", sid, name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::ingestion::now_rfc3339;

    fn test_source_row(root: Option<&str>, url: Option<&str>) -> SourceRow {
        SourceRow {
            id: "01HRQHB7FN3WMX4AZDV3S9VCTZ".to_string(),
            store_id: "store-1".to_string(),
            kind: if root.is_some() {
                SourceKind::Path
            } else {
                SourceKind::Url
            },
            root: root.map(str::to_string),
            url: url.map(str::to_string),
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: now_rfc3339(),
        }
    }

    #[test]
    fn format_source_line_single_store_matches_legacy_format() {
        let src = test_source_row(Some("/Volumes/Archive/books"), None);
        let line = format_source_line(None, 0, &src);
        assert_eq!(
            line,
            "01HRQHB7FN3WMX4AZDV3S9VCTZ [path] /Volumes/Archive/books"
        );
    }

    #[test]
    fn format_source_line_multi_store_prefixes_padded_name() {
        let src = test_source_row(Some("/Volumes/Archive/books"), None);
        let width = store_column_width(["books", "default"].into_iter());
        assert_eq!(width, 9); // "default" (7) + 2
        let line = format_source_line(Some("books"), width, &src);
        assert_eq!(
            line,
            "books    01HRQHB7FN3WMX4AZDV3S9VCTZ [path] /Volumes/Archive/books"
        );
    }

    #[test]
    fn format_source_line_falls_back_to_url_when_no_root() {
        let src = test_source_row(None, Some("https://example.com"));
        let line = format_source_line(None, 0, &src);
        assert_eq!(line, "01HRQHB7FN3WMX4AZDV3S9VCTZ [url] https://example.com");
    }

    #[test]
    fn store_column_width_uses_longest_name_plus_two() {
        assert_eq!(store_column_width(["a", "bb", "ccc"].into_iter()), 5);
        assert_eq!(store_column_width(std::iter::empty()), 2);
    }
}
