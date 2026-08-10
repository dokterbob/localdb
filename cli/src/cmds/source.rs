use std::sync::Arc;

use localdb_core::{
    ids::new_ulid, ingestion::now_rfc3339, source::normalize_path_source, types::SourceKind,
    Embedder, Error, SourceRow, StoreRow,
};
use serde_json::json;

use crate::{
    app_db::{load_app_db, resolve_daemon_store_scope, resolve_store_scope, StoreScopePolicy},
    cmds::index::{run_embedded_index_with, IndexErrorMode},
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{
        classify_source, exit_err, exit_err_with_partial_results, kind_to_string, looks_like_id,
        print_json,
    },
};

/// Resolve the effective source kind for `source add` / `add` and, for feed
/// sources, the parsed spec — pure and side-effect free so the exit-code-2
/// flag-matrix rejections (issue #116) are unit testable without going
/// through `exit_err`'s `process::exit`.
///
/// `--kind` overrides `classify_source` uniformly for all three kinds (an
/// explicit `--kind path`/`--kind url` also bypasses classification);
/// `classify_source` itself stays two-way and is only consulted when no
/// override is given. `--max-entries` / `--no-fetch-full-content` are
/// feed-only flags, rejected here for any other kind. Feed validation
/// itself (http(s) requirement, `max_entries != 0`) is centralized in
/// `parse_source_spec`'s `"feed"` arm — the single validation authority —
/// rather than duplicated here.
pub(crate) fn resolve_source_add_kind(
    source_arg: &str,
    kind_override: Option<&str>,
    max_entries: Option<u32>,
    no_fetch_full_content: bool,
) -> Result<(String, Option<localdb_core::source::ParsedSourceSpec>), Error> {
    let kind: String =
        kind_override.map_or_else(|| classify_source(source_arg).0.to_string(), String::from);

    if max_entries.is_some() && kind != "feed" {
        return Err(Error::InvalidRequest {
            message: "--max-entries is only supported for feed sources (--kind feed)".to_string(),
        });
    }
    if no_fetch_full_content && kind != "feed" {
        return Err(Error::InvalidRequest {
            message: "--no-fetch-full-content is only supported for feed sources (--kind feed)"
                .to_string(),
        });
    }

    // An explicit `--kind url` bypasses `classify_source`, which is what
    // normally guarantees a url-kind arg is `http(s)://`-shaped. Without this
    // check, `source add /tmp/docs --kind url` would persist (exit 0) a url
    // source whose locator can never parse — auto-index only warns, so the
    // source would sit permanently unindexable. Full parse, not a prefix
    // check (`https://[` and bare `https://` pass a prefix check but can
    // never parse), mirroring the feed arm's validation; `--kind path` stays
    // unrestricted (any string can be a path).
    if kind == "url" && kind_override.is_some() {
        let scheme_ok = localdb_core::uri::Uri::parse(source_arg)
            .is_some_and(|u| matches!(u.scheme(), "http" | "https"));
        if !scheme_ok {
            return Err(Error::InvalidRequest {
                message: format!("url source must be a valid http(s) URL: '{source_arg}'"),
            });
        }
    }

    if kind == "feed" {
        let feed_spec = json!({
            "url": source_arg,
            "max_entries": max_entries,
            "fetch_full_content": !no_fetch_full_content,
        });
        let parsed = localdb_core::source::parse_source_spec("feed", &feed_spec)?;
        Ok((kind, Some(parsed)))
    } else {
        Ok((kind, None))
    }
}

/// `localdb source add <path-or-url>`
#[allow(clippy::too_many_arguments)]
pub fn run_source_add(
    ctx: &CliContext,
    source_arg: &str,
    refresh: Option<&str>,
    kind_override: Option<&str>,
    max_entries: Option<u32>,
    no_fetch_full_content: bool,
) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_add_async(
        ctx,
        source_arg,
        refresh,
        kind_override,
        max_entries,
        no_fetch_full_content,
    ));
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_source_add_async(
    ctx: &CliContext,
    source_arg: &str,
    refresh: Option<&str>,
    kind_override: Option<&str>,
    max_entries: Option<u32>,
    no_fetch_full_content: bool,
) {
    let (kind, parsed_feed_spec) = match resolve_source_add_kind(
        source_arg,
        kind_override,
        max_entries,
        no_fetch_full_content,
    ) {
        Ok(v) => v,
        Err(e) => exit_err(&e, ctx.json),
    };
    let kind = kind.as_str();
    let fetch_full_content = !no_fetch_full_content;

    let (config_loader, db) = load_app_db(ctx).await;

    // Per specs/05-surfaces.md §2: route to daemon when running. Probed before
    // store resolution because the two paths resolve scope differently — the
    // daemon owns its own store set (see `resolve_daemon_store_scope`).
    let daemon_state = probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref());

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

    if refresh.is_some() && kind != "url" && kind != "feed" {
        exit_err(
            &Error::InvalidRequest {
                message: "refresh is only supported for URL and feed sources".to_string(),
            },
            ctx.json,
        );
    }

    if let DaemonState::Running { ref base_url } = daemon_state {
        // Ask the daemon which stores actually exist rather than treating
        // `--store` as pre-validated names (Codex review round 2, findings 1
        // & 4) — a running daemon may point at an entirely different data
        // directory than this process would otherwise open.
        let store_names =
            resolve_daemon_store_scope(base_url, ctx, StoreScopePolicy::DefaultStore).await;

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
            } else if kind == "feed" {
                json!({
                    "url": source_arg,
                    "max_entries": max_entries,
                    "fetch_full_content": fetch_full_content,
                })
            } else {
                json!({ "url": source_arg })
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
                Err(e) => {
                    // Finding 5: don't discard results already persisted by
                    // earlier iterations of this loop — see
                    // `exit_err_with_partial_results`'s doc comment. Non-JSON
                    // mode already printed each success as it happened, so
                    // it keeps using plain `exit_err`.
                    if ctx.json {
                        exit_err_with_partial_results(&e, json_results);
                    } else {
                        exit_err(&e, ctx.json);
                    }
                }
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

    // Sources that were added locally and need auto-indexing, run in a
    // second pass below once every source in this request has been
    // persisted.
    let mut to_index: Vec<(StoreRow, String)> = Vec::new();

    // Accumulate JSON results across the loop and emit exactly one top-level
    // document afterward (finding 3) — see the daemon branch above for the
    // same restructuring and its rationale.
    let mut json_results: Vec<serde_json::Value> = Vec::new();

    for row in &rows {
        let src = if kind == "feed" {
            // #116: already validated + parsed by `resolve_source_add_kind`
            // above (routed through `parse_source_spec`, the single
            // validation authority) — reuse it rather than re-parsing.
            // Fields are cloned per store since the same parsed spec is
            // reused across every store in scope.
            let parsed = parsed_feed_spec
                .as_ref()
                .expect("feed kind always yields a parsed spec");
            SourceRow {
                id: new_ulid(),
                store_id: row.id.clone(),
                kind: parsed.kind.clone(),
                root: parsed.root.clone(),
                url: parsed.url.clone(),
                include: parsed.include.clone(),
                exclude: parsed.exclude.clone(),
                preset: "prose".to_string(),
                refresh: refresh.map(|s| s.to_string()),
                created_at: now_rfc3339(),
                config_json: parsed.config_json.clone(),
            }
        } else {
            SourceRow {
                id: new_ulid(),
                store_id: row.id.clone(),
                // `classify_source`/`resolve_source_add_kind` only ever
                // yield "url" or "path" here (feed is handled above), but
                // `kind` is a `&str`, so a `match` would need an
                // unreachable wildcard arm. Two branches keep it honest and
                // coverable.
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
                url: if kind == "path" {
                    None
                } else {
                    Some(source_arg.to_string())
                },
                include: include_globs.clone(),
                exclude: exclude_globs.clone(),
                preset: "prose".to_string(),
                refresh: refresh.map(|s| s.to_string()),
                created_at: now_rfc3339(),
                config_json: None,
            }
        };

        if let Err(e) = db.backend().upsert_source(&src).await {
            // Finding 5: don't discard results already persisted by earlier
            // iterations of this loop — see
            // `exit_err_with_partial_results`'s doc comment. Non-JSON mode
            // already printed each success as it happened, so it keeps using
            // plain `exit_err`.
            if ctx.json {
                exit_err_with_partial_results(&e, json_results);
            } else {
                exit_err(&e, ctx.json);
            }
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
        if kind == "path" || kind == "url" || kind == "feed" {
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

    // Auto-index every newly added source, reusing the already-open
    // `db`/`config_loader` and threading the built embedder across stores so
    // an N-store `source add` builds the (potentially ~706 MB local)
    // embedder at most once rather than once per store (Codex review round
    // 2, finding 6) — the same threading `run_index_async` does for
    // `localdb index`.
    let mut embedder: Option<Arc<dyn Embedder>> = None;
    for (row, src_id) in &to_index {
        if !ctx.json {
            eprintln!("Auto-indexing source {} ...", src_id);
        }
        let (_summary, used_embedder) = match run_embedded_index_with(
            ctx,
            &db,
            &config_loader,
            row,
            Some(src_id),
            IndexErrorMode::WarnAndContinue,
            embedder.clone(),
            None,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => exit_err(&e, ctx.json),
        };
        if embedder.is_none() {
            embedder = used_embedder;
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
            .map(|(store_name, s)| source_to_json_value(s, store_name))
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
/// column. `col_width` is only consulted when `store_name` is `Some`. A thin
/// wrapper over `source_to_human_line`, which owns the actual per-kind
/// rendering (including feed detail, #116) — this function only adds the
/// store-name column.
fn format_source_line(store_name: Option<&str>, col_width: usize, src: &SourceRow) -> String {
    match store_name {
        Some(name) => format!(
            "{:<width$}{}",
            name,
            source_to_human_line(src),
            width = col_width
        ),
        None => source_to_human_line(src),
    }
}

/// Build one `source list --json` row. Feed sources get their parsed (never
/// raw `config_json`) `max_entries` / `fetch_full_content` fields; `refresh`
/// is surfaced for both url and feed sources (#116).
pub(crate) fn source_to_json_value(s: &SourceRow, store_name: &str) -> serde_json::Value {
    let mut obj = json!({
        "id": s.id,
        "store": { "name": store_name },
        "store_id": s.store_id,
        "kind": kind_to_string(&s.kind),
        "root": s.root,
        "url": s.url,
        "preset": s.preset,
    });
    if matches!(s.kind, SourceKind::Url | SourceKind::Feed) {
        obj["refresh"] = json!(s.refresh);
    }
    if s.kind == SourceKind::Feed {
        let feed_config = localdb_core::source::parse_feed_config_json(s.config_json.as_deref());
        obj["max_entries"] = json!(feed_config.max_entries);
        obj["fetch_full_content"] = json!(feed_config.fetch_full_content);
    }
    obj
}

/// Build one `source list` human-readable line.
///
/// Feed rows get an extra `(max_entries=…, full_content=on|off)` suffix
/// (`max_entries=unbounded` when uncapped) — path/url rows are unchanged
/// (#116).
pub(crate) fn source_to_human_line(s: &SourceRow) -> String {
    let loc = s.root.as_deref().or(s.url.as_deref()).unwrap_or("?");
    if s.kind == SourceKind::Feed {
        let feed_config = localdb_core::source::parse_feed_config_json(s.config_json.as_deref());
        let max_entries_str = feed_config
            .max_entries
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unbounded".to_string());
        let full_content_str = if feed_config.fetch_full_content {
            "on"
        } else {
            "off"
        };
        format!(
            "{} [{}] {} (max_entries={}, full_content={})",
            s.id,
            kind_to_string(&s.kind),
            loc,
            max_entries_str,
            full_content_str
        )
    } else {
        format!("{} [{}] {}", s.id, kind_to_string(&s.kind), loc)
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
        // validate directly (not via `resolve_daemon_store_scope`) because
        // that helper's empty-input case resolves an implicit `default`
        // scope, which is meaningless for remove-by-ID — there's no
        // per-store scope to inject here, only syntax-checking of whatever
        // `--store` values were actually passed. Nor do we ask the daemon to
        // confirm these names exist (see the KNOWN LIMITATION note below).
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
        // `resolve_daemon_store_scope`'s doc comment in `cli/src/app_db.rs`).
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
            config_json: None,
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

    fn feed_row(
        id: &str,
        url: &str,
        config_json: Option<&str>,
        refresh: Option<&str>,
    ) -> SourceRow {
        SourceRow {
            id: id.to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Feed,
            root: None,
            url: Some(url.to_string()),
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: refresh.map(str::to_string),
            created_at: now_rfc3339(),
            config_json: config_json.map(str::to_string),
        }
    }

    fn url_row(id: &str, url: &str, refresh: Option<&str>) -> SourceRow {
        SourceRow {
            id: id.to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Url,
            root: None,
            url: Some(url.to_string()),
            include: vec![],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: refresh.map(str::to_string),
            created_at: now_rfc3339(),
            config_json: None,
        }
    }

    fn path_row(id: &str, root: &str) -> SourceRow {
        SourceRow {
            id: id.to_string(),
            store_id: "store-1".to_string(),
            kind: SourceKind::Path,
            root: Some(root.to_string()),
            url: None,
            include: vec!["**/*.md".to_string()],
            exclude: vec![],
            preset: "prose".to_string(),
            refresh: None,
            created_at: now_rfc3339(),
            config_json: None,
        }
    }

    // --- resolve_source_add_kind: flag-matrix rejections (exit 2) ---

    #[test]
    fn resolve_source_add_kind_rejects_max_entries_with_path_kind() {
        let err = resolve_source_add_kind("/tmp/docs", Some("path"), Some(10), false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[test]
    fn resolve_source_add_kind_rejects_max_entries_with_url_kind() {
        let err = resolve_source_add_kind("https://example.com/page", Some("url"), Some(10), false)
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn resolve_source_add_kind_rejects_max_entries_without_override_on_inferred_path() {
        // No --kind at all: classify_source infers "path" from a non-URL arg.
        let err = resolve_source_add_kind("/tmp/docs", None, Some(5), false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn resolve_source_add_kind_rejects_no_fetch_full_content_with_non_feed() {
        let err = resolve_source_add_kind("/tmp/docs", Some("path"), None, true).unwrap_err();
        assert_eq!(err.exit_code(), 2);

        let err = resolve_source_add_kind("https://example.com/page", Some("url"), None, true)
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn resolve_source_add_kind_rejects_kind_feed_non_http_url() {
        let err = resolve_source_add_kind("ftp://example.com/feed.xml", Some("feed"), None, false)
            .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[test]
    fn resolve_source_add_kind_rejects_max_entries_zero() {
        let err =
            resolve_source_add_kind("https://example.com/feed.xml", Some("feed"), Some(0), false)
                .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    // --- resolve_source_add_kind: acceptance paths ---

    #[test]
    fn resolve_source_add_kind_accepts_feed_defaults() {
        let (kind, parsed) =
            resolve_source_add_kind("https://example.com/feed.xml", Some("feed"), None, false)
                .unwrap();
        assert_eq!(kind, "feed");
        let parsed = parsed.expect("feed kind yields a parsed spec");
        assert_eq!(parsed.kind, SourceKind::Feed);
        assert_eq!(parsed.url, Some("https://example.com/feed.xml".to_string()));
        let config = localdb_core::source::parse_feed_config_json(parsed.config_json.as_deref());
        assert_eq!(config.max_entries, None);
        assert!(config.fetch_full_content);
    }

    #[test]
    fn resolve_source_add_kind_accepts_feed_with_explicit_fields() {
        let (kind, parsed) =
            resolve_source_add_kind("https://example.com/feed.xml", Some("feed"), Some(25), true)
                .unwrap();
        assert_eq!(kind, "feed");
        let parsed = parsed.unwrap();
        let config = localdb_core::source::parse_feed_config_json(parsed.config_json.as_deref());
        assert_eq!(config.max_entries, Some(25));
        assert!(
            !config.fetch_full_content,
            "--no-fetch-full-content flips the default"
        );
    }

    #[test]
    fn resolve_source_add_kind_infers_path_and_url_without_override() {
        let (kind, parsed) = resolve_source_add_kind("/tmp/docs", None, None, false).unwrap();
        assert_eq!(kind, "path");
        assert!(parsed.is_none());

        let (kind, parsed) =
            resolve_source_add_kind("https://example.com/page", None, None, false).unwrap();
        assert_eq!(kind, "url");
        assert!(parsed.is_none());
    }

    #[test]
    fn resolve_source_add_kind_override_bypasses_classification() {
        // A URL-shaped string can be forced to "path": #116 says `--kind`
        // overrides classification uniformly. (The reverse — forcing a
        // non-URL string to "url" — is rejected; see the scheme-check tests
        // below.)
        let (kind, _) =
            resolve_source_add_kind("https://example.com/page", Some("path"), None, false).unwrap();
        assert_eq!(kind, "path");
    }

    #[test]
    fn resolve_source_add_kind_rejects_kind_url_non_http_arg() {
        // Explicit `--kind url` bypasses classify_source's http(s) shape
        // guarantee; without a scheme check it would persist a url source
        // that can never be indexed (auto-index only warns, exit 0).
        let err = resolve_source_add_kind("/tmp/docs", Some("url"), None, false).unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
        assert!(err.to_string().contains("must be a valid http(s) URL"));
    }

    #[test]
    fn resolve_source_add_kind_rejects_kind_url_unparseable_http_prefixed_arg() {
        // Right prefix, but not a parseable URL (unclosed IPv6 bracket /
        // empty host) — a prefix-only check would persist these.
        for bad in ["https://[", "https://", "http://"] {
            let err = resolve_source_add_kind(bad, Some("url"), None, false).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest { .. }),
                "expected InvalidRequest for arg={bad}"
            );
        }
    }

    #[test]
    fn resolve_source_add_kind_accepts_kind_url_http_arg() {
        let (kind, parsed) =
            resolve_source_add_kind("https://example.com/page", Some("url"), None, false).unwrap();
        assert_eq!(kind, "url");
        assert!(parsed.is_none());
    }

    // --- source list formatting ---

    #[test]
    fn source_to_human_line_feed_with_max_entries() {
        let row = feed_row(
            "src-1",
            "https://example.com/feed.xml",
            Some(r#"{"max_entries":25,"fetch_full_content":false}"#),
            None,
        );
        let line = source_to_human_line(&row);
        assert_eq!(
            line,
            "src-1 [feed] https://example.com/feed.xml (max_entries=25, full_content=off)"
        );
    }

    #[test]
    fn source_to_human_line_feed_unbounded_defaults() {
        let row = feed_row("src-2", "https://example.com/feed.xml", None, None);
        let line = source_to_human_line(&row);
        assert_eq!(
            line,
            "src-2 [feed] https://example.com/feed.xml (max_entries=unbounded, full_content=on)"
        );
    }

    #[test]
    fn source_to_human_line_path_and_url_unchanged() {
        let row = path_row("src-3", "/tmp/docs");
        assert_eq!(source_to_human_line(&row), "src-3 [path] /tmp/docs");

        let row = url_row("src-4", "https://example.com/page", None);
        assert_eq!(
            source_to_human_line(&row),
            "src-4 [url] https://example.com/page"
        );
    }

    #[test]
    fn source_to_json_value_feed_includes_parsed_fields_and_refresh_not_raw_config_json() {
        let row = feed_row(
            "src-5",
            "https://example.com/feed.xml",
            Some(r#"{"max_entries":10,"fetch_full_content":false}"#),
            Some("1h"),
        );
        let v = source_to_json_value(&row, "notes");
        assert_eq!(v["kind"], "feed");
        assert_eq!(v["max_entries"], 10);
        assert_eq!(v["fetch_full_content"], false);
        assert_eq!(v["refresh"], "1h");
        // Never expose the raw config_json blob.
        assert!(v.get("config_json").is_none());
    }

    #[test]
    fn source_to_json_value_url_surfaces_refresh_but_no_feed_fields() {
        let row = url_row("src-6", "https://example.com/page", Some("30m"));
        let v = source_to_json_value(&row, "notes");
        assert_eq!(v["kind"], "url");
        assert_eq!(v["refresh"], "30m");
        assert!(v.get("max_entries").is_none());
        assert!(v.get("fetch_full_content").is_none());
    }

    #[test]
    fn source_to_json_value_path_has_no_refresh_field() {
        let row = path_row("src-7", "/tmp/docs");
        let v = source_to_json_value(&row, "notes");
        assert_eq!(v["kind"], "path");
        assert!(v.get("refresh").is_none());
    }

    // -- auto-index embedder reuse (Codex review round 2, finding 6) --------

    /// `source add` scoped to two stores must build the (potentially ~706 MB
    /// local) embedder once for the whole request, not once per store.
    ///
    /// Drives `run_source_add_async` end to end against a real temp DB/config
    /// (provider `fake`, so it's fully offline and cheap) and asserts on
    /// `crate::cmds::index::EMBEDDER_BUILD_COUNT`, a test-only counter
    /// incremented exactly where `run_embedded_index_with` calls
    /// `embed::create_embedder`. Before the fix, `source add`'s auto-index
    /// loop called the single-store `run_embedded_index` wrapper once per
    /// store, rebuilding the embedder each time; this test fails red against
    /// that code (count == 2 for two stores) and green once the loop threads
    /// one `Arc<dyn Embedder>` across stores via `run_embedded_index_with`,
    /// exactly as `run_index_async` already does for `localdb index`.
    #[tokio::test]
    async fn source_add_across_two_stores_builds_embedder_once() {
        use crate::cmds::index::EMBEDDER_BUILD_COUNT;
        use crate::cmds::store::run_store_add_async;
        use std::sync::atomic::Ordering;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let note_path = dir.path().join("note.md");
        std::fs::write(&note_path, "# Hello\n\nSome content to index.\n").unwrap();

        let config_path = dir.path().join("config.yaml");
        std::fs::write(
            &config_path,
            format!(
                "version: 1\npaths:\n  data: {}\ndefaults:\n  indexing:\n    embedding:\n      provider: fake\n      model: bge-small-en-v1.5\n",
                dir.path().display()
            ),
        )
        .unwrap();

        let base_ctx = CliContext {
            config: Some(config_path.clone()),
            json: false,
            stores: vec![],
            yes: false,
            daemon_url: None,
            config_env: None,
        };
        // Pre-create both stores: `source add`'s explicit `--store` scope
        // requires them to already exist (`resolve_store_scope_inner`).
        run_store_add_async(&base_ctx, "a").await;
        run_store_add_async(&base_ctx, "b").await;

        // Reset just before the call under test: no other test in this crate
        // currently drives `run_embedded_index_with`'s embedder-construction
        // path, so this is safe against `cargo test`'s parallel test threads
        // (see the counter's doc comment).
        EMBEDDER_BUILD_COUNT.store(0, Ordering::SeqCst);

        let add_ctx = CliContext {
            config: Some(config_path),
            json: false,
            stores: vec!["a".to_string(), "b".to_string()],
            yes: false,
            daemon_url: None,
            config_env: None,
        };
        run_source_add_async(
            &add_ctx,
            note_path.to_str().unwrap(),
            None,
            None,
            None,
            false,
        )
        .await;

        assert_eq!(
            EMBEDDER_BUILD_COUNT.load(Ordering::SeqCst),
            1,
            "auto-indexing 2 stores in one `source add` must build the embedder once, not once per store"
        );
    }
}
