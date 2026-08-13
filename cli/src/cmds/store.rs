use localdb_core::{config::loader::ConfigLoader, Error};
use serde_json::json;

use crate::{
    app_db::{
        apply_daemon_store_scope, default_store_row, load_config_lenient, load_config_scaffolded,
        open_app_db_lenient_or_exit, open_app_db_or_exit, reject_store_flag,
        resolve_store_scope_inner, AppDb, StoreScopePolicy, STORE_ADD_REJECT_MESSAGE,
        STORE_REMOVE_REJECT_MESSAGE,
    },
    command_table::{dispatch, DaemonAwareCommand},
    daemon_client::{daemon_request_async, encode_path_segment, walk_daemon_pages, CliContext},
    normalize::{
        confirm_destructive, exit_err, print_json, validate_store_name, visibility_to_string,
    },
};

// ---------------------------------------------------------------------------
// store add
// ---------------------------------------------------------------------------

/// The mode-agnostic result of `store add` — rendered identically regardless
/// of transport (issue #187 stage 5). The daemon-only `(via daemon)` suffix
/// the old hand-written branch printed is gone: mode is not part of the
/// result.
pub(crate) struct StoreAddOutcome {
    pub(crate) name: String,
    pub(crate) id: String,
}

pub(crate) struct StoreAddCmd<'a> {
    pub(crate) name: &'a str,
}

impl DaemonAwareCommand for StoreAddCmd<'_> {
    type Outcome = StoreAddOutcome;

    // `store add` names its store as a positional argument; `--store` is
    // rejected outright before `dispatch` ever runs (see `run_store_add_async`
    // below), so no variant here is ever consulted. `AllStores` is as good a
    // placeholder as any.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, _ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        let url = format!("{base_url}/v1/stores");
        let body = json!({ "name": self.name, "visibility": "private", "backend": "libsql" });
        let v = daemon_request_async(reqwest::Method::POST, &url, Some(body)).await?;
        let name = v
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or(self.name)
            .to_string();
        let id = v
            .get("id")
            .and_then(|i| i.as_str())
            .ok_or_else(|| Error::Internal {
                message: "daemon store-add response missing 'id'".to_string(),
                correlation_id: "daemon_store_add_shape".to_string(),
            })?
            .to_string();
        Ok(StoreAddOutcome { name, id })
    }

    async fn run_embedded(
        &self,
        _ctx: &CliContext,
        _config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        if db.backend().get_store_by_name(self.name).await?.is_some() {
            return Err(Error::InvalidRequest {
                message: format!("store '{}' already exists", self.name),
            });
        }

        let store = default_store_row(self.name, db)?;
        db.backend().upsert_store(&store).await?;
        Ok(StoreAddOutcome {
            name: self.name.to_string(),
            id: store.id,
        })
    }
}

fn render_store_add(outcome: StoreAddOutcome, json_mode: bool) {
    if json_mode {
        print_json(&json!({ "status": "ok", "name": outcome.name, "id": outcome.id }));
    } else {
        println!("Added store: {}", outcome.name);
    }
}

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

    let config_loader = load_config_scaffolded(ctx).await;
    let outcome = dispatch(&StoreAddCmd { name }, ctx, &config_loader, || {
        open_app_db_or_exit(ctx, &config_loader)
    })
    .await;
    render_store_add(outcome, ctx.json);
}

// ---------------------------------------------------------------------------
// store list
// ---------------------------------------------------------------------------

/// One store, as `store list` reports it — identical fields whether sourced
/// from the embedded DB or a daemon's `GET /v1/stores`.
#[derive(Clone)]
pub(crate) struct StoreListEntry {
    pub(crate) name: String,
    pub(crate) visibility: String,
    pub(crate) backend: String,
}

pub(crate) struct StoreListCmd;

impl DaemonAwareCommand for StoreListCmd {
    type Outcome = Vec<StoreListEntry>;

    // specs/05-surfaces.md §2.2: `--store` filters; omitted -> every store.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        // H2 (Codex review, PR #212): validate every requested name for
        // traversal-safety *before* `walk_daemon_pages` fires the first `GET
        // /v1/stores` request — mirrors the in-`run_daemon` loop idiom
        // `source remove` uses (`SourceRemoveCmd::run_daemon` above).
        // Previously `apply_daemon_store_scope` below was the only
        // validation, running only after the daemon had already been
        // queried.
        for name in &ctx.stores {
            validate_store_name(name)?;
        }
        let mut all: Vec<StoreListEntry> = Vec::new();
        walk_daemon_pages(base_url, "/v1/stores", |items| {
            for item in items {
                all.push(StoreListEntry {
                    name: item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    visibility: item
                        .get("visibility")
                        .and_then(|n| n.as_str())
                        .unwrap_or("private")
                        .to_string(),
                    backend: item
                        .get("backend")
                        .and_then(|n| n.as_str())
                        .unwrap_or("?")
                        .to_string(),
                });
            }
            false
        })
        .await?;
        apply_daemon_store_scope(&all, |s| s.name.as_str(), ctx, Self::SCOPE_POLICY)
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        _config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        let runtime_stores = resolve_store_scope_inner(ctx, db, Self::SCOPE_POLICY).await?;
        Ok(runtime_stores
            .into_iter()
            .map(|s| StoreListEntry {
                name: s.name,
                visibility: visibility_to_string(&s.visibility).to_string(),
                backend: s.backend,
            })
            .collect())
    }
}

fn render_store_list(outcome: Vec<StoreListEntry>, json_mode: bool) {
    if json_mode {
        let all: Vec<serde_json::Value> = outcome
            .iter()
            .map(|s| json!({ "name": s.name, "visibility": s.visibility, "backend": s.backend }))
            .collect();
        print_json(&json!({ "stores": all }));
    } else if outcome.is_empty() {
        println!("No stores.");
    } else {
        for s in &outcome {
            println!("{} [{}]", s.name, s.backend);
        }
    }
}

/// `localdb store list`
pub fn run_store_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_store_list_async(ctx));
}

pub(crate) async fn run_store_list_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so store list works even with malformed config.
    let config_loader = load_config_lenient(ctx).await;
    let outcome = dispatch(&StoreListCmd, ctx, &config_loader, || {
        open_app_db_lenient_or_exit(ctx, &config_loader)
    })
    .await;
    render_store_list(outcome, ctx.json);
}

// ---------------------------------------------------------------------------
// store remove
// ---------------------------------------------------------------------------

/// The mode-agnostic result of `store remove` (issue #187 stage 5) — the
/// `(via daemon)` suffix the old daemon branch printed is gone.
pub(crate) struct StoreRemoveOutcome {
    pub(crate) name: String,
}

pub(crate) struct StoreRemoveCmd<'a> {
    pub(crate) name: &'a str,
}

impl DaemonAwareCommand for StoreRemoveCmd<'_> {
    type Outcome = StoreRemoveOutcome;

    // `store remove` names its store as a positional argument; `--store` is
    // rejected before `dispatch` runs (see `run_store_remove_async`), so no
    // variant here is ever consulted.
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStores;

    async fn run_daemon(&self, _ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        // `name` is percent-encoded before it's interpolated into the URL
        // path segment — see `daemon_client::encode_path_segment`'s doc
        // comment (finding 1): an unescaped '#'/'?'/'/' would otherwise
        // retarget the DELETE at a different daemon endpoint entirely.
        let url = format!("{base_url}/v1/stores/{}", encode_path_segment(self.name));
        daemon_request_async(reqwest::Method::DELETE, &url, None).await?;
        Ok(StoreRemoveOutcome {
            name: self.name.to_string(),
        })
    }

    async fn run_embedded(
        &self,
        _ctx: &CliContext,
        _config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        let store_id = db.resolve_store_id(self.name).await?;
        if db.backend().delete_store(&store_id).await? {
            Ok(StoreRemoveOutcome {
                name: self.name.to_string(),
            })
        } else {
            Err(Error::StoreNotFound {
                id: self.name.to_string(),
            })
        }
    }
}

fn render_store_remove(outcome: StoreRemoveOutcome, json_mode: bool) {
    if json_mode {
        print_json(&json!({ "status": "ok", "name": outcome.name }));
    } else {
        println!("Removed store: {}", outcome.name);
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

    // H2 (Codex review, PR #212): validate the store name before anything
    // else — like `store add` does in `run_store_add_async` above — so both
    // embedded and daemon mode reject a syntactically invalid name
    // (`InvalidRequest`/exit 2) instead of only embedded mode doing so via a
    // later "not found" lookup, and so the user is never prompted about a
    // name that can never exist. This intentionally changes embedded mode's
    // exit code for names like `../bad` from 3 (StoreNotFound) to 2.
    if let Err(e) = validate_store_name(name) {
        exit_err(&e, ctx.json);
    }

    let config_loader = load_config_scaffolded(ctx).await;

    let prompt = format!(
        "This permanently deletes store '{}', its sources, and its index data. Continue?",
        name
    );
    if !confirm_destructive(ctx, &prompt) {
        return;
    }

    let outcome = dispatch(&StoreRemoveCmd { name }, ctx, &config_loader, || {
        open_app_db_or_exit(ctx, &config_loader)
    })
    .await;
    render_store_remove(outcome, ctx.json);
}
