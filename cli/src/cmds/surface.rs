use localdb_core::Error;
use serde_json::json;

use crate::{
    app_db::{
        load_config_scaffolded, load_config_scaffolded_local, open_app_db_or_exit,
        reject_store_flag, resolve_store_scope, StoreScopePolicy, SERVE_REJECT_MESSAGE,
    },
    daemon_client::{probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json, visibility_to_string},
};

/// `localdb serve` — start the HTTP daemon (specs/05-surfaces.md §3).
pub fn run_serve(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_serve_async(ctx));
}

pub(crate) async fn run_serve_async(ctx: &CliContext) {
    // specs/05-surfaces.md §2.2: the daemon serves every store in the
    // database — `/v1` and `/mcp` alike — so there is nothing for `-s` to
    // narrow. First statement in the function so a misused flag exits before
    // `create_dir_all`/`start_daemon` bind a port or take the write lock.
    reject_store_flag(ctx, SERVE_REJECT_MESSAGE);

    // Issue #119/#120: `serve` is itself a legitimate first-run entry point
    // (nothing requires `localdb init` before `localdb serve`), so it now
    // scaffolds config + a `default` store on a genuine first run, the same
    // way the strict `command_table::dispatch` call sites do — see
    // `app_db::load_config_scaffolded`'s doc comment. The `_local` variant
    // because `serve` never routes to `LOCALDB_DAEMON_URL` — it always
    // starts a local daemon — so the env var must not suppress the local
    // `default`-store seeding the way it does for routable commands (see
    // `load_config_scaffolded_local`'s doc comment). Its scaffolding errors
    // (e.g. the F11 guard on an explicit `--config` with a missing parent)
    // map to the same exit codes the old bare `load_config` hard-fail below
    // did: `Error::InvalidConfig` -> exit 2, via the same `exit_err`.
    let config_loader = load_config_scaffolded_local(ctx).await;

    // Still required even after scaffolding: `ensure_config_scaffolded` only
    // creates `paths.data`/`models`/`logs` on a genuine first run (no config
    // file at the resolved path at all) — when a config file already exists
    // but names a data dir that hasn't been created yet (e.g. a hand-edited
    // `paths.data`), scaffolding is a no-op and this is still the only thing
    // that creates it. Right after a fresh scaffold, `data_dir` already
    // exists, so this is a no-op `create_dir_all` in that case.
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
    use mcp::{
        proxy::{ProxyConnectError, ProxyHandler},
        AvailableStore, McpHandler, StoreDescriptor,
    };

    // specs/05-surfaces.md §4: v1 registers no mutating tool on any
    // transport, so `--allow-write` currently changes nothing — the tool set
    // is identical with and without it. Warn rather than exit 2 (which is
    // what a misapplied `-s` gets): this flag fails *safe*. It can only
    // withhold a capability the caller would notice immediately as a missing
    // tool, whereas `-s` failing open would silently widen access. Refusing
    // to start an MCP server over it would be disproportionate.
    if allow_write {
        eprintln!(
            "warning: no mutating MCP tools exist in v1; `--allow-write` currently has no effect"
        );
    }

    // Config only, up front: `probe_daemon` only needs
    // `config_loader.paths.data_dir`, and `mcp` is hand-rolled rather than a
    // `command_table::dispatch` call site, so it adopts the same lazy-open
    // helpers dispatch's call sites use (issue #187 review, finding G4) by
    // hand. The local `AppDb` is opened below, via `open_app_db_or_exit`,
    // only in the embedded branch — never in the `Proxied` branch, which
    // never touches it. Before this, `load_app_db` opened the local db
    // unconditionally, so a broken local store (unwritable, locked,
    // schema-too-new) would `exit_err` before `probe_daemon` ever ran,
    // preempting a healthy daemon that never needed the local db at all.
    let config_loader = load_config_scaffolded(ctx).await;

    if let DaemonState::Running { base_url } =
        probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref())
    {
        // Connect and serve are separate calls (`ProxyHandler::connect` then
        // `mcp::serve_proxied_stdio`, rather than one moded entrypoint) so a
        // failure to reach the daemon at all — it went away between
        // `probe_daemon` and here, or `LOCALDB_DAEMON_URL` points at a stale
        // endpoint — maps to the same `daemon_unreachable`/exit-5 outcome as
        // every other daemon-backed CLI path, instead of `internal`/exit-1.
        // Only a failure in the stdio loop *after* a successful proxy
        // connection (a much rarer case) still falls back to `internal`.
        //
        // `ctx.stores` is passed through rather than warned about: proxied
        // mode now genuinely enforces `--store` (specs/05-surfaces.md
        // §4.2.1). `connect` validates each name against the store set the
        // daemon actually exposes over MCP, so an unknown name is
        // `store_not_found`/exit 3 here — the same answer embedded mode
        // gives — instead of the old behavior of warning and then serving
        // the daemon's *full* store set, which silently widened access
        // exactly when the caller had asked to narrow it (#201).
        //
        // Syntax-validate first, though (Codex review, P2). `connect` can
        // only ever answer "the daemon doesn't have that name", so a
        // malformed one like `../evil` would surface as
        // `store_not_found`/exit 3 — while embedded mode and every other
        // store-scoped command reject it as `invalid_request`/exit 2 before
        // resolving anything. Same ordering, and the same reason, as
        // `resolve_daemon_store_scope_inner` and `source remove`'s daemon
        // branch: a malformed name never reaches the wire.
        for name in &ctx.stores {
            if let Err(e) = crate::normalize::validate_store_name(name) {
                exit_err(&e, ctx.json);
            }
        }

        let handler = match ProxyHandler::connect(&base_url, &ctx.stores).await {
            Ok(handler) => handler,
            Err(ProxyConnectError::StoreNotFound(name)) => {
                exit_err(&Error::StoreNotFound { id: name }, ctx.json);
            }
            Err(ProxyConnectError::Unreachable(e)) => {
                // The underlying transport/handshake error (`e`) is otherwise
                // discarded — `exit_err` below only ever prints the generic
                // "daemon is unreachable" message, giving no clue *why* the
                // proxy hop failed (issue #147: the daemon/MCP connection
                // path gives no diagnostic signal on rejection). `warn!` so
                // it surfaces under the default `warn,pdf_oxide=off` filter
                // (`localdb/src/main.rs`) without needing `RUST_LOG=debug`.
                tracing::warn!(
                    daemon_url = %base_url,
                    error = %e,
                    "mcp proxy: failed to connect to daemon"
                );
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

    // Same store resolution as `localdb search`, through the one shared
    // resolver (specs/05-surfaces.md §2.2/§4.2.1). This replaced a
    // hand-rolled `runtime_stores.iter().find(...)` loop that *skipped*
    // unmatched `--store` names: `localdb -s typo mcp` used to start a server
    // exposing zero stores, which reads to an agent as "this index is empty"
    // rather than "you typo'd" (#201). It is now `store_not_found`, exit 3.
    //
    // `AllStoresAllowEmpty`, not `AllStores`: a genuinely storeless database
    // must still *start* — an MCP server that exits non-zero at startup reads
    // to its client as broken, not as empty.
    let db = open_app_db_or_exit(ctx, &config_loader).await;
    let scoped_stores = resolve_store_scope(ctx, &db, StoreScopePolicy::AllStoresAllowEmpty).await;

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
    for store_row in &scoped_stores {
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

    let handler = McpHandler::new(available, std::sync::Arc::from(embedder), allow_write);

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
