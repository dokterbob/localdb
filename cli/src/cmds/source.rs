use std::sync::Arc;

use localdb_core::{
    config::loader::ConfigLoader, ids::new_ulid, ingestion::now_rfc3339, ingestion::DeletionPolicy,
    source::normalize_path_source, types::SourceKind, Embedder, Error, IndexJobScope, SourceRow,
    StoreRow,
};
use serde_json::json;
use server::JobQueue;

use crate::{
    app_db::{
        load_config_scaffolded, open_app_db_or_exit, resolve_daemon_store_scope,
        resolve_store_scope, resolve_store_scope_inner, AppDb, StoreScopePolicy,
    },
    cmds::index::IndexErrorMode,
    command_table::{dispatch, DaemonAwareCommand},
    daemon_client::{daemon_request_async, encode_path_segment, walk_daemon_pages, CliContext},
    job_attach,
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

// ---------------------------------------------------------------------------
// source add
// ---------------------------------------------------------------------------
//
// `source add`'s daemon/embedded branches were already unified onto the
// shared async job model in issue #187 stage 3 (auto-index runs through
// `job_attach::run_daemon_store_job` / `run_embedded_store_job` either way,
// with identical `IndexErrorMode::WarnAndContinue` semantics). Stage 5 moved
// the *mode selection* itself onto `command_table::dispatch` so a future edit
// can't reintroduce a second, competing `probe_daemon` call.
//
// Adversarial review (issue #187, finding 1): the two branches' *rendering*
// had NOT actually converged — `run_daemon` printed the raw daemon-persisted
// `SourceRecord` echo (`--json`) and a `(via daemon)` text suffix, while
// `run_embedded` printed a hand-built `{id, kind, status, store}` object and
// plain text. Fixed by reducing both branches to the same `AddedSource`
// triple and routing every print through `render_source_add_item` /
// `render_source_add_summary` — the only functions in this command that call
// `println!`/`print_json`. Canonical shape = embedded mode's pre-existing
// output, byte-for-byte; the daemon branch converges to it, and the `(via
// daemon)` suffix / raw `SourceRecord` echo are both gone.
//
// `Outcome = ()`: unlike `search`/`store list`/`status`, this command's
// output is inherently streaming — non-JSON mode prints each store's result
// as soon as it's persisted, and a mid-loop `--json` failure must flush
// whatever succeeded so far via `exit_err_with_partial_results` *before*
// aborting, not after a final value is assembled. Both of those need to
// happen from inside the loop, which rules out collecting into one value
// rendered afterward — so unlike `render_source_remove` (which renders once,
// after `dispatch` returns), `render_source_add_item` is called from inside
// BOTH transports' loops. The requirement finding 1 fixes is that the
// printing code carry no mode branch, not that output be buffered to the
// end.

/// The subset of `source add`'s arguments needed by both transports,
/// resolved once by `run_source_add_async` (kind classification, feed spec
/// parsing, path normalization, refresh validation) before `dispatch` ever
/// runs — so `run_daemon`/`run_embedded` can't each re-derive it and drift.
struct SourceAddCmd<'a> {
    source_arg: &'a str,
    kind: &'a str,
    parsed_feed_spec: Option<&'a localdb_core::source::ParsedSourceSpec>,
    actual_root: &'a str,
    include_globs: &'a [String],
    exclude_globs: &'a [String],
    refresh: Option<&'a str>,
    max_entries: Option<u32>,
    fetch_full_content: bool,
}

/// One added source's outcome, as `source add` renders it — the fields
/// `render_source_add_item`/`render_source_add_summary` need, whichever
/// transport produced them. `kind` is always one of `self.kind`'s three
/// values ("path"/"url"/"feed"), the same strings `kind_to_string` yields, so
/// both transports can populate it directly without a `SourceKind` round
/// trip.
struct AddedSource {
    id: String,
    store_name: String,
    kind: String,
}

/// The one per-item renderer for `source add`'s `Ok` path — called from
/// inside BOTH transports' loops (issue #187 review, finding 1), immediately
/// after each source is persisted. Text mode prints the line right away;
/// `--json` mode instead accumulates into `json_results` for
/// `render_source_add_summary` (or `exit_err_with_partial_results`) to emit
/// later — see the module doc comment above `SourceAddCmd` for why this
/// can't be deferred to a single end-of-loop call the way `source remove`'s
/// `render_source_remove` is.
fn render_source_add_item(
    added: &AddedSource,
    ctx: &CliContext,
    json_results: &mut Vec<serde_json::Value>,
) {
    if ctx.json {
        json_results.push(json!({
            "id": added.id,
            "store": { "name": added.store_name },
            "kind": added.kind,
        }));
    } else {
        println!("Added source {} to store '{}'", added.id, added.store_name);
    }
}

/// The one `--json`-summary renderer for `source add`, called once after
/// both transports' loops complete without error. No-op in text mode — text
/// mode already printed every line via `render_source_add_item`.
fn render_source_add_summary(json_results: &[serde_json::Value], json_mode: bool) {
    if !json_mode {
        return;
    }
    if json_results.len() == 1 {
        // Single store: today's exact flat shape — specs/05-surfaces.md
        // §2.2 promises existing scripts don't break.
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

impl DaemonAwareCommand for SourceAddCmd<'_> {
    type Outcome = ();

    // `source add` is the one write command whose omitted-`--store` scope is
    // a single implicit `default` store, not "every store" — see
    // `StoreScopePolicy::DefaultStore`'s doc comment.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::DefaultStore;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<(), Error> {
        // Ask the daemon which stores actually exist rather than treating
        // `--store` as pre-validated names (Codex review round 2, findings 1
        // & 4) — a running daemon may point at an entirely different data
        // directory than this process would otherwise open.
        let store_names = resolve_daemon_store_scope(base_url, ctx, Self::SCOPE_POLICY).await;

        // Accumulate JSON results across the loop and emit exactly one
        // top-level document afterward (finding 3): printing per-iteration,
        // as this used to, made `--store a --store b --json source add`
        // write multiple back-to-back JSON objects to stdout, which isn't
        // parseable as a single document.
        let mut json_results: Vec<serde_json::Value> = Vec::new();

        // Sources successfully added, queued for a second-pass auto-index
        // (D3, issue #187 stage 3) once every source in this request has
        // been persisted — mirrors the embedded branch's own `to_index`
        // deferral below.
        let mut to_index: Vec<(String, String)> = Vec::new();

        for store_name in &store_names {
            // The handler's CreateSourceRequest expects {kind, spec, preset}
            // where spec is a nested object (see server/src/handlers.rs
            // CreateSourceRequest).
            let spec = if self.kind == "path" {
                json!({ "root": self.actual_root, "include": self.include_globs, "exclude": self.exclude_globs })
            } else if self.kind == "feed" {
                json!({
                    "url": self.source_arg,
                    "max_entries": self.max_entries,
                    "fetch_full_content": self.fetch_full_content,
                })
            } else {
                json!({ "url": self.source_arg })
            };
            // `store_name` is percent-encoded before it's interpolated into
            // the URL path segment — an unescaped '#'/'?'/'/' would otherwise
            // retarget the request at a different daemon endpoint entirely
            // (finding 1: e.g. a store named "a#b" would silently POST to
            // `/v1/stores/a`, since '#' starts a URL fragment that's never
            // sent to the server).
            let url_str = format!(
                "{}/v1/stores/{}/sources",
                base_url,
                encode_path_segment(store_name)
            );
            let body = json!({
                "kind": self.kind,
                "spec": spec,
                "preset": "prose",
                "refresh": self.refresh,
            });
            match daemon_request_async(reqwest::Method::POST, &url_str, Some(body)).await {
                Ok(v) => {
                    // Only `id` is pulled from the daemon's response — the
                    // rest of its raw persisted `SourceRecord` echo (spec,
                    // include/exclude globs, preset, ...) never reaches the
                    // renderer (finding 1): the daemon converges on
                    // embedded mode's reduced `AddedSource` shape instead of
                    // leaking its own storage representation.
                    let new_id = v
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if !new_id.is_empty()
                        && (self.kind == "path" || self.kind == "url" || self.kind == "feed")
                    {
                        to_index.push((store_name.clone(), new_id.clone()));
                    }
                    render_source_add_item(
                        &AddedSource {
                            id: new_id,
                            store_name: store_name.clone(),
                            kind: self.kind.to_string(),
                        },
                        ctx,
                        &mut json_results,
                    );
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

        render_source_add_summary(&json_results, ctx.json);

        // D3 (issue #187 stage 3): the daemon branch used to skip
        // auto-indexing entirely — a source added while a daemon was
        // running sat unindexed until a later `localdb index`. Now it
        // submits a best-effort auto-index job per newly-added source, the
        // same `IndexErrorMode::WarnAndContinue` semantics the embedded
        // branch below has always had: a failure (submission, attach, or
        // the job itself) only ever warns to stderr, never fails `source
        // add` (`job_attach::run_daemon_store_job` never returns `Err`
        // under `WarnAndContinue`, short of the defensive
        // non-terminal-state case, which is likewise swallowed here).
        for (store_name, src_id) in &to_index {
            if !ctx.json {
                eprintln!("Auto-indexing source {} ...", src_id);
            }
            let _ = job_attach::run_daemon_store_job(
                ctx,
                base_url,
                store_name,
                Some(src_id.as_str()),
                DeletionPolicy::Retain,
                IndexErrorMode::WarnAndContinue,
                None,
            )
            .await;
        }
        Ok(())
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<(), Error> {
        // specs/05-surfaces.md §2.2: bare invocation -> store named "default";
        // `-s` (repeatable) always wins and is validated/resolved/deduped here.
        // `source add` is the *one* command that still defaults this way — every
        // other `-s`-accepting command treats the flag as a filter over all
        // stores. A write has to pick a single target, and "every store" would
        // fan one `source add` out across the whole database; picking `default`
        // by name is the only choice here that isn't a guess.
        let rows = resolve_store_scope(ctx, db, Self::SCOPE_POLICY).await;

        // Sources that were added locally and need auto-indexing, run in a
        // second pass below once every source in this request has been
        // persisted.
        let mut to_index: Vec<(StoreRow, String)> = Vec::new();

        // Accumulate JSON results across the loop and emit exactly one top-level
        // document afterward (finding 3) — see the daemon branch above for the
        // same restructuring and its rationale.
        let mut json_results: Vec<serde_json::Value> = Vec::new();

        for row in &rows {
            let src = if self.kind == "feed" {
                // #116: already validated + parsed by `resolve_source_add_kind`
                // above (routed through `parse_source_spec`, the single
                // validation authority) — reuse it rather than re-parsing.
                // Fields are cloned per store since the same parsed spec is
                // reused across every store in scope.
                let parsed = self
                    .parsed_feed_spec
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
                    refresh: self.refresh.map(|s| s.to_string()),
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
                    kind: if self.kind == "url" {
                        SourceKind::Url
                    } else {
                        SourceKind::Path
                    },
                    root: if self.kind == "path" {
                        Some(self.actual_root.to_string())
                    } else {
                        None
                    },
                    url: if self.kind == "path" {
                        None
                    } else {
                        Some(self.source_arg.to_string())
                    },
                    include: self.include_globs.to_vec(),
                    exclude: self.exclude_globs.to_vec(),
                    preset: "prose".to_string(),
                    refresh: self.refresh.map(|s| s.to_string()),
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

            render_source_add_item(
                &AddedSource {
                    id: src.id.clone(),
                    store_name: row.name.clone(),
                    kind: kind_to_string(&src.kind).to_string(),
                },
                ctx,
                &mut json_results,
            );

            // #2: Auto-index after source add.
            if self.kind == "path" || self.kind == "url" || self.kind == "feed" {
                to_index.push((row.clone(), src.id.clone()));
            }
        }

        render_source_add_summary(&json_results, ctx.json);

        // Auto-index every newly added source, reusing the already-open
        // `db`/`config_loader` and threading the built embedder across stores so
        // an N-store `source add` builds the (potentially ~706 MB local)
        // embedder at most once rather than once per store (Codex review round
        // 2, finding 6) — the same threading `run_index_async` does for
        // `localdb index`. Runs through the same unified job model
        // (`job_attach::run_embedded_store_job`, issue #187 stage 3) `localdb
        // index` uses, via a local `JobQueue` scoped to this command; under
        // `IndexErrorMode::WarnAndContinue` it never returns `Err`, so a failure
        // only ever warns to stderr and `source add` itself always succeeds.
        let mut embedder: Option<Arc<dyn Embedder>> = None;
        let queue = JobQueue::new();
        for (row, src_id) in &to_index {
            if !ctx.json {
                eprintln!("Auto-indexing source {} ...", src_id);
            }
            let _ = job_attach::run_embedded_store_job(
                ctx,
                &queue,
                config_loader,
                db,
                row,
                IndexJobScope::Source {
                    source_id: src_id.clone(),
                },
                // A source being added for the first time has no indexed history
                // to prune, and auto-index is not the place to remove anything.
                DeletionPolicy::Retain,
                IndexErrorMode::WarnAndContinue,
                &mut embedder,
                None,
            )
            .await;
        }
        Ok(())
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

    let config_loader = load_config_scaffolded(ctx).await;

    // Normalize path sources: validate existence, promote single files, apply
    // excludes. Store-independent, so this runs once regardless of how many
    // stores are in scope, and once regardless of which transport ends up
    // handling the request.
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

    let cmd = SourceAddCmd {
        source_arg,
        kind,
        parsed_feed_spec: parsed_feed_spec.as_ref(),
        actual_root: &actual_root,
        include_globs: &include_globs,
        exclude_globs: &exclude_globs,
        refresh,
        max_entries,
        fetch_full_content,
    };
    dispatch(&cmd, ctx, &config_loader, || {
        open_app_db_or_exit(ctx, &config_loader)
    })
    .await;
}

// ---------------------------------------------------------------------------
// source list
// ---------------------------------------------------------------------------

/// One source, as `source list` reports it — identical fields whether
/// sourced from an embedded `SourceRow` or a daemon's `GET
/// /v1/stores/{name}/sources` (issue #187 stage 5, decision D2: reads route
/// to the daemon when one is detected).
struct SourceListItem {
    id: String,
    store_id: String,
    store_name: String,
    kind: String,
    root: Option<String>,
    url: Option<String>,
    preset: String,
    refresh: Option<String>,
    max_entries: Option<u32>,
    fetch_full_content: Option<bool>,
}

fn source_row_to_list_item(s: &SourceRow, store_name: &str) -> SourceListItem {
    let (max_entries, fetch_full_content) = if s.kind == SourceKind::Feed {
        let feed_config = localdb_core::source::parse_feed_config_json(s.config_json.as_deref());
        (
            feed_config.max_entries,
            Some(feed_config.fetch_full_content),
        )
    } else {
        (None, None)
    };
    SourceListItem {
        id: s.id.clone(),
        store_id: s.store_id.clone(),
        store_name: store_name.to_string(),
        kind: kind_to_string(&s.kind).to_string(),
        root: s.root.clone(),
        url: s.url.clone(),
        preset: s.preset.clone(),
        refresh: s.refresh.clone(),
        max_entries,
        fetch_full_content,
    }
}

/// Convert one raw `GET /v1/stores/{name}/sources` item (a `SourceRecord`,
/// `server/src/state.rs`) into a `SourceListItem`. `spec` shapes vary by kind
/// (`{"root",...}` for path, `{"url"}` for url, `{"url","max_entries",
/// "fetch_full_content"}` for feed — see `server::state::source_row_to_record`),
/// so fields are read defensively rather than assumed present.
fn daemon_item_to_list_item(item: &serde_json::Value, store_name: &str) -> SourceListItem {
    let kind = item
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("path")
        .to_string();
    let spec = item.get("spec").cloned().unwrap_or(json!({}));
    let max_entries = if kind == "feed" {
        spec.get("max_entries")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
    } else {
        None
    };
    let fetch_full_content = if kind == "feed" {
        Some(
            spec.get("fetch_full_content")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
        )
    } else {
        None
    };
    SourceListItem {
        id: item
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("?")
            .to_string(),
        store_id: item
            .get("store_id")
            .and_then(|i| i.as_str())
            .unwrap_or("?")
            .to_string(),
        store_name: store_name.to_string(),
        kind,
        root: spec
            .get("root")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        url: spec.get("url").and_then(|v| v.as_str()).map(str::to_string),
        preset: item
            .get("preset")
            .and_then(|p| p.as_str())
            .unwrap_or("prose")
            .to_string(),
        refresh: item
            .get("refresh")
            .and_then(|r| r.as_str())
            .map(str::to_string),
        max_entries,
        fetch_full_content,
    }
}

struct SourceListCmd;

impl DaemonAwareCommand for SourceListCmd {
    // The resolved scope's store *names* alongside the items themselves
    // (issue #187 review, finding 1) — the store-name column / "no sources
    // on store X" message need every store *resolved* into scope, not just
    // the subset that happened to return at least one item. Folding items
    // from `--store populated --store empty` down to `Vec<SourceListItem>`
    // alone loses `empty` entirely, so a caller reconstructing scope size
    // from `items` would wrongly see one store in scope and drop the column.
    // Returning both from the one dispatch call (rather than a second
    // dispatch just to re-resolve names) also avoids doubling the daemon
    // round-trip cost for every `source list` invocation, not just the
    // previously-special-cased empty one.
    type Outcome = (Vec<String>, Vec<SourceListItem>);

    // specs/05-surfaces.md §2.2: `-s` is a *filter* — a bare `source list`
    // spans every store.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        let store_names =
            crate::app_db::resolve_daemon_store_scope_inner(base_url, ctx, Self::SCOPE_POLICY)
                .await?;

        let mut all = Vec::new();
        for store_name in &store_names {
            let path = format!("/v1/stores/{}/sources", encode_path_segment(store_name));
            walk_daemon_pages(base_url, &path, |items| {
                for item in items {
                    all.push(daemon_item_to_list_item(item, store_name));
                }
                false
            })
            .await?;
        }
        Ok((store_names, all))
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        _config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        let rows = resolve_store_scope_inner(ctx, db, Self::SCOPE_POLICY).await?;
        let mut all = Vec::new();
        for row in &rows {
            let sources = db.backend().list_sources(&row.id).await?;
            for s in &sources {
                all.push(source_row_to_list_item(s, &row.name));
            }
        }
        let store_names = rows.into_iter().map(|r| r.name).collect();
        Ok((store_names, all))
    }
}

/// Width of the store-name column: longest name in scope plus two spaces of
/// separation before the source line begins. Only used when `>1` store is in
/// scope; callers pass `0` (ignored) otherwise.
fn store_column_width<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    names.map(str::len).max().unwrap_or(0) + 2
}

/// Build one `source list --json` row. Feed sources get their parsed
/// `max_entries` / `fetch_full_content` fields; `refresh` is surfaced for
/// both url and feed sources (#116).
///
/// D4/D2 (issue #187 stages 3/5): the `store` field is emitted
/// unconditionally, matching the pre-existing embedded behavior — the spec's
/// "only when more than one store is in scope" claim (§2.3) is itself wrong
/// and gets corrected in the docs stage, not here (no behavior churn).
///
/// `store_id` (issue #187 review, finding 2) sits alongside `store.name` —
/// pre-existing embedded behavior (and documented in `docs/cli.md`'s worked
/// example) that the shared renderer dropped when both transports were
/// unified onto it; restored here so it's populated by both.
fn source_list_item_json(s: &SourceListItem) -> serde_json::Value {
    let mut obj = json!({
        "id": s.id,
        "store": { "name": s.store_name },
        "store_id": s.store_id,
        "kind": s.kind,
        "root": s.root,
        "url": s.url,
        "preset": s.preset,
    });
    if s.kind == "url" || s.kind == "feed" {
        obj["refresh"] = json!(s.refresh);
    }
    if s.kind == "feed" {
        obj["max_entries"] = json!(s.max_entries);
        obj["fetch_full_content"] = json!(s.fetch_full_content);
    }
    obj
}

/// Format a single `source list` line, with or without a leading store-name
/// column. `col_width` is only consulted when `with_store_column` is true.
fn source_list_item_human_line(
    s: &SourceListItem,
    with_store_column: bool,
    col_width: usize,
) -> String {
    let loc = s.root.as_deref().or(s.url.as_deref()).unwrap_or("?");
    let body = if s.kind == "feed" {
        let max_entries_str = s
            .max_entries
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unbounded".to_string());
        let full_content_str = if s.fetch_full_content.unwrap_or(true) {
            "on"
        } else {
            "off"
        };
        format!(
            "{} [{}] {} (max_entries={}, full_content={})",
            s.id, s.kind, loc, max_entries_str, full_content_str
        )
    } else {
        format!("{} [{}] {}", s.id, s.kind, loc)
    };
    if with_store_column {
        format!("{:<width$}{}", s.store_name, body, width = col_width)
    } else {
        body
    }
}

fn render_source_list(items: &[SourceListItem], scope_store_names: &[String], json_mode: bool) {
    if json_mode {
        let json_sources: Vec<serde_json::Value> =
            items.iter().map(source_list_item_json).collect();
        print_json(&json!({ "sources": json_sources }));
        return;
    }

    if items.is_empty() {
        if scope_store_names.len() == 1 {
            println!("No sources on store '{}'.", scope_store_names[0]);
        } else {
            println!("No sources in scope.");
        }
        return;
    }

    // Output gains a store-name column only when more than one store is in
    // scope; a single store in scope keeps the pre-existing output format
    // (specs/05-surfaces.md §2.2).
    let multi = scope_store_names.len() > 1;
    let col_width = if multi {
        store_column_width(scope_store_names.iter().map(String::as_str))
    } else {
        0
    };
    for s in items {
        println!("{}", source_list_item_human_line(s, multi, col_width));
    }
}

/// `localdb source list`
pub fn run_source_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_list_async(ctx));
}

pub(crate) async fn run_source_list_async(ctx: &CliContext) {
    let config_loader = load_config_scaffolded(ctx).await;
    // The store-name column / "no sources on store X" message key off the
    // *resolved scope's* store names (`scope_store_names`), never off which
    // of them happened to return an item (issue #187 review, finding 1) — a
    // scope of `--store populated --store empty` must still show the column
    // on `populated`'s line, not silently collapse to a single-store-looking
    // scope just because `empty` contributed nothing to `items`.
    let (scope_store_names, items) = dispatch(&SourceListCmd, ctx, &config_loader, || {
        open_app_db_or_exit(ctx, &config_loader)
    })
    .await;
    render_source_list(&items, &scope_store_names, ctx.json);
}

// ---------------------------------------------------------------------------
// source remove
// ---------------------------------------------------------------------------

/// One deleted source, as `source remove` reports it. `store_name` is
/// `None` for the daemon transport: `DELETE /v1/sources/{id}` is
/// store-agnostic (see the `KNOWN LIMITATION` note in `SourceRemoveCmd::run_daemon`)
/// and never tells the CLI which store the source belonged to. This is never
/// user-visible: the daemon path always deletes exactly one source (a single
/// DELETE per invocation), which is exactly the case embedded mode's own
/// single-item rendering already omits the store name for.
struct DeletedSource {
    id: String,
    store_name: Option<String>,
}

struct SourceRemoveCmd<'a> {
    id: &'a str,
}

impl DaemonAwareCommand for SourceRemoveCmd<'_> {
    type Outcome = Vec<DeletedSource>;

    // `source remove`'s embedded scope defaults to "every store" (a bare
    // invocation with a globally-unique ULID spans all of them) — see
    // `run_source_remove_async`'s doc comment on the ULID/path distinction.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        // Finding 5: validate --store names for traversal-safety before the
        // DELETE fires, matching `source add`'s daemon branch. We validate
        // directly (not via `resolve_daemon_store_scope`) because that
        // helper's empty-input case resolves an implicit `default` scope,
        // which is meaningless for remove-by-ID — there's no per-store scope
        // to inject here, only syntax-checking of whatever `--store` values
        // were actually passed. Nor do we ask the daemon to confirm these
        // names exist (see the KNOWN LIMITATION note below).
        for name in &ctx.stores {
            crate::normalize::validate_store_name(name)?;
        }

        // KNOWN LIMITATION (issue #188): `DELETE /v1/sources/{id}` is
        // store-agnostic, so daemon mode has no way to enforce that the
        // source actually belongs to a store named by `--store` — embedded
        // mode does enforce this (see `run_embedded`'s `matches` resolution,
        // D2). Fixing that needs an HTTP API change; tracked in #188, not
        // attempted here. We deliberately do NOT add a local existence check
        // for `--store` either: `LOCALDB_DAEMON_URL` may point at a daemon on
        // another host with its own data directory, so a syntactically-valid
        // but locally-unknown store name must still reach the daemon (see
        // `resolve_daemon_store_scope`'s doc comment in `cli/src/app_db.rs`).
        // `id` is percent-encoded before it's interpolated into the URL path
        // segment — see the `encode_path_segment` doc comment (finding 1);
        // same class of bug as the store-name case above.
        let url = format!("{base_url}/v1/sources/{}", encode_path_segment(self.id));
        daemon_request_async(reqwest::Method::DELETE, &url, None).await?;
        Ok(vec![DeletedSource {
            id: self.id.to_string(),
            store_name: None,
        }])
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        _config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        // specs/05-surfaces.md §2.2: a bare invocation spans every store.
        // Safe for both argument shapes by this point — the path/url case
        // already exited 2 above (in `run_source_remove_async`) demanding
        // `-s`, so only a globally-unique ULID reaches an implicit scope, and
        // scoping *that* to `default` only made valid ids fail when their
        // store happened not to be `default` (#201).
        let rows = resolve_store_scope_inner(ctx, db, Self::SCOPE_POLICY).await?;

        // Resolve (store, source_id) matches within the scoped stores.
        let matches: Vec<(StoreRow, String)> =
            if looks_like_id(self.id) {
                // A global ID is inherently single-store: fetch it once, then
                // check that the store it actually belongs to is in scope (D2).
                let src = db.backend().get_source(self.id).await?.ok_or_else(|| {
                    Error::SourceNotFound {
                        id: self.id.to_string(),
                    }
                })?;
                match rows.iter().find(|r| r.id == src.store_id) {
                    Some(row) => vec![(row.clone(), src.id)],
                    None => {
                        return Err(Error::SourceNotFound {
                            id: self.id.to_string(),
                        })
                    }
                }
            } else {
                // Path/url: look it up per resolved store; a matching root/url
                // can in principle exist in more than one store in scope.
                let mut found = Vec::new();
                for row in &rows {
                    if let Some(src) = db
                        .backend()
                        .find_source_by_root_or_url(self.id, &row.id)
                        .await?
                    {
                        found.push((row.clone(), src.id));
                    }
                }
                if found.is_empty() {
                    return Err(Error::SourceNotFound {
                        id: self.id.to_string(),
                    });
                }
                found
            };

        let mut deleted = Vec::with_capacity(matches.len());
        for (row, source_id) in &matches {
            if db.backend().delete_source(source_id).await? {
                deleted.push(DeletedSource {
                    id: source_id.clone(),
                    store_name: Some(row.name.clone()),
                });
            } else {
                return Err(Error::SourceNotFound {
                    id: source_id.clone(),
                });
            }
        }
        Ok(deleted)
    }
}

/// The one renderer for `source remove`'s `Outcome`. Both transports collapse
/// to the same single-item shape in the common case: the daemon always
/// returns exactly one `DeletedSource`, and embedded mode's own single-match
/// rendering already omits the store name — so this doesn't need a
/// mode-specific branch to reach parity, it just always was structurally
/// compatible with a single generic renderer (issue #187 stage 5). The
/// `(via daemon)` suffix the old daemon branch printed is gone: mode is not
/// part of the result.
fn render_source_remove(deleted: &[DeletedSource], json_mode: bool) {
    if json_mode {
        if deleted.len() == 1 {
            print_json(&json!({ "status": "ok", "id": deleted[0].id }));
        } else {
            let results: Vec<serde_json::Value> = deleted
                .iter()
                .map(|d| {
                    json!({
                        "id": d.id,
                        "store": { "name": d.store_name.clone().unwrap_or_default() },
                    })
                })
                .collect();
            print_json(&json!({ "status": "ok", "results": results }));
        }
    } else if deleted.len() == 1 {
        println!("Removed source: {}", deleted[0].id);
    } else {
        for d in deleted {
            println!(
                "Removed source: {} from store '{}'",
                d.id,
                d.store_name.as_deref().unwrap_or("?")
            );
        }
    }
}

/// `localdb source remove <id-or-path-or-url>`
pub fn run_source_remove(ctx: &CliContext, id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_source_remove_async(ctx, id));
}

pub(crate) async fn run_source_remove_async(ctx: &CliContext, id: &str) {
    let config_loader = load_config_scaffolded(ctx).await;

    // #3: If the argument looks like a path or URL (not a ULID/UUID), it must
    // be resolved against a specific store's sources, so an explicit --store
    // is required. This is the one place `source remove`'s two implicit-scope
    // rules diverge (specs/05-surfaces.md §2.2): a ULID is globally unique, so
    // a bare invocation can safely span every store, but the *same* path can
    // be a source in several stores at once — deleting from all of them on a
    // bare `source remove ~/notes` would be a guess with teeth. Checked here,
    // before any scope resolution, so the two rules never interact.
    if !looks_like_id(id) && ctx.stores.is_empty() {
        exit_err(
            &Error::InvalidRequest {
                message: "source remove by path/url requires --store; pass --store <name> or use the source ULID".into(),
            },
            ctx.json,
        );
    }

    let deleted = dispatch(&SourceRemoveCmd { id }, ctx, &config_loader, || {
        open_app_db_or_exit(ctx, &config_loader)
    })
    .await;
    render_source_remove(&deleted, ctx.json);
}

/// Build one `source list` human-readable line — kept for direct unit
/// testing below, delegating to the `SourceListItem`-based renderer that
/// `run_source_list_async` actually uses.
#[cfg(test)]
fn source_to_human_line(s: &SourceRow) -> String {
    source_list_item_human_line(&source_row_to_list_item(s, "?"), false, 0)
}

#[cfg(test)]
fn source_to_json_value(s: &SourceRow, store_name: &str) -> serde_json::Value {
    source_list_item_json(&source_row_to_list_item(s, store_name))
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
        let line = source_to_human_line(&src);
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
        let item = source_row_to_list_item(&src, "books");
        let line = source_list_item_human_line(&item, true, width);
        assert_eq!(
            line,
            "books    01HRQHB7FN3WMX4AZDV3S9VCTZ [path] /Volumes/Archive/books"
        );
    }

    #[test]
    fn format_source_line_falls_back_to_url_when_no_root() {
        let src = test_source_row(None, Some("https://example.com"));
        let line = source_to_human_line(&src);
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
