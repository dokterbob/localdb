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
        classify_source, exit_err, kind_to_string, looks_like_id, print_json, validate_store_name,
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

    if refresh.is_some() && kind != "url" && kind != "feed" {
        exit_err(
            &Error::InvalidRequest {
                message: "refresh is only supported for URL and feed sources".to_string(),
            },
            ctx.json,
        );
    }

    let src = if kind == "feed" {
        // #116: already validated + parsed by `resolve_source_add_kind`
        // above (routed through `parse_source_spec`, the single validation
        // authority) — reuse it rather than re-parsing.
        let parsed = parsed_feed_spec.expect("feed kind always yields a parsed spec");
        SourceRow {
            id: new_ulid(),
            store_id: rt_store.id.clone(),
            kind: parsed.kind,
            root: parsed.root,
            url: parsed.url,
            include: parsed.include,
            exclude: parsed.exclude,
            preset: "prose".to_string(),
            refresh: refresh.map(|s| s.to_string()),
            created_at: now_rfc3339(),
            config_json: parsed.config_json,
        }
    } else {
        SourceRow {
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
            url: if kind == "path" {
                None
            } else {
                Some(source_arg.to_string())
            },
            include: include_globs,
            exclude: exclude_globs,
            preset: "prose".to_string(),
            refresh: refresh.map(|s| s.to_string()),
            created_at: now_rfc3339(),
            config_json: None,
        }
    };

    if let Err(e) = db.backend().upsert_source(&src).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "id": src.id,
            "store": { "name": store_name },
            "kind": kind_to_string(&src.kind),
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

    if kind == "path" || kind == "url" || kind == "feed" {
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
            .map(|s| source_to_json_value(s, &store_name))
            .collect();
        print_json(&json!({ "sources": json_sources }));
    } else if sources.is_empty() {
        println!("No sources on store '{}'.", store_name);
    } else {
        for s in &sources {
            println!("{}", source_to_human_line(s));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // A URL-shaped string can be forced to "path" and vice versa: #116
        // says `--kind` overrides classification uniformly.
        let (kind, _) =
            resolve_source_add_kind("https://example.com/page", Some("path"), None, false).unwrap();
        assert_eq!(kind, "path");
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
}
