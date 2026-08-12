//! Declarative command table (issue #187 stage 5).
//!
//! Every command that has both an embedded and a daemon-routed
//! implementation drifted, one command at a time, because each hand-rolled
//! its own probe -> branch -> render sequence (issue #187 §3). This module
//! replaces that pattern with one trait, [`DaemonAwareCommand`], and one
//! function, [`dispatch`], that is the *only* place a command's transport is
//! chosen.
//!
//! The invariant this buys: a command has exactly one [`DaemonAwareCommand::Outcome`]
//! type. `run_daemon` and `run_embedded` each build one from their own
//! transport, but neither prints anything — printing happens once, at the
//! call site, from whichever `Outcome` `dispatch` returned. There is
//! structurally no way for the daemon branch to render something the
//! embedded branch wouldn't, because rendering isn't part of either branch.
//!
//! `SCOPE_POLICY` exists for the same reason: several commands resolve a
//! `--store` scope (via `resolve_store_scope`/`resolve_daemon_store_scope`)
//! inside both `run_daemon` and `run_embedded`, and those two resolutions
//! must apply the *same* policy or an omitted `--store` could mean "every
//! store" under one transport and "exit 2, no stores" under the other. A
//! single associated constant, consulted by both methods, makes that
//! agreement structural rather than something a future edit could silently
//! break in only one branch.

use std::future::Future;

use localdb_core::{config::loader::ConfigLoader, Error};

use crate::app_db::{AppDb, StoreScopePolicy};
use crate::daemon_client::{probe_daemon, CliContext, DaemonState};
use crate::normalize::exit_err;

/// A CLI command whose embedded and daemon-routed implementations must
/// produce identical results.
///
/// Implementors are typically small structs holding the command's own
/// parsed arguments (e.g. `store add`'s store name) — the actual work lives
/// in `run_daemon`/`run_embedded`, not in the struct.
pub(crate) trait DaemonAwareCommand {
    /// The mode-agnostic domain value both transports produce, consumed by
    /// exactly one renderer at the call site.
    type Outcome;

    /// How this command resolves an omitted `--store` flag — see the module
    /// doc comment. Commands that don't resolve a store scope at all (none
    /// currently on the table, but a future one might) can pick any variant;
    /// it simply goes unused.
    const SCOPE_POLICY: StoreScopePolicy;

    /// Run this command against a running daemon at `base_url`.
    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error>;

    /// Run this command against the embedded store, using the caller's
    /// `config_loader` and a `db` that `dispatch` opened lazily — only once
    /// the daemon probe came back `NotRunning` (issue #187 review, finding
    /// G4). Which loader produced `config_loader` (`load_config_for_maintenance`
    /// vs. its lenient counterpart) predates and is orthogonal to
    /// daemon-vs-embedded routing, so that choice remains the caller's
    /// responsibility, not `dispatch`'s — only the *timing* of the DB open
    /// moved.
    async fn run_embedded(
        &self,
        ctx: &CliContext,
        config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error>;
}

/// Probe for a running daemon and route to exactly one of `C::run_daemon` /
/// `C::run_embedded` — the only place in the CLI a table-driven command's
/// transport is chosen.
///
/// `open_db` is called at most once, and only from the `NotRunning` arm,
/// immediately before `run_embedded` — never from the `Running` arm (issue
/// #187 review, finding G4). Before this, every call site opened the local
/// `AppDb` *before* calling `dispatch`, so a broken local store (unwritable,
/// locked, schema-too-new) would `exit_err` on the open and never reach the
/// daemon probe at all — a healthy daemon was preempted by a local DB that
/// route never needed. `open_db` is infallible (`FnOnce() -> Future<Output =
/// AppDb>`, not `Result`) because opening still exits the process on failure
/// via `exit_err`, exactly as it always did — only the timing changed, not
/// the error handling: callers pass a closure like `|| open_app_db_or_exit(ctx,
/// &config_loader)` (or its lenient counterpart), and both already exit
/// internally.
///
/// Exits the process (via `exit_err`) on any error from either transport, so
/// callers only ever see `C::Outcome` on return — feed it straight to the
/// command's one renderer.
pub(crate) async fn dispatch<C, F, Fut>(
    cmd: &C,
    ctx: &CliContext,
    config_loader: &ConfigLoader,
    open_db: F,
) -> C::Outcome
where
    C: DaemonAwareCommand,
    F: FnOnce() -> Fut,
    Fut: Future<Output = AppDb>,
{
    match probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref()) {
        DaemonState::Running { base_url } => match cmd.run_daemon(ctx, &base_url).await {
            Ok(outcome) => outcome,
            Err(e) => exit_err(&e, ctx.json),
        },
        DaemonState::NotRunning => {
            let db = open_db().await;
            match cmd.run_embedded(ctx, config_loader, &db).await {
                Ok(outcome) => outcome,
                Err(e) => exit_err(&e, ctx.json),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::config::loader::ResolvedPaths;
    use localdb_core::config::schema::{
        DefaultsConfig, EmbeddingPolicy, PathsConfig, RawConfig, ServerConfig,
    };
    use tempfile::TempDir;

    /// A minimal `DaemonAwareCommand` whose two branches return distinct,
    /// directly-observable outcomes — enough to prove `dispatch` routed to
    /// the right one without needing a real HTTP daemon.
    struct ProbeCmd;

    impl DaemonAwareCommand for ProbeCmd {
        type Outcome = &'static str;
        const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStoresAllowEmpty;

        async fn run_daemon(
            &self,
            _ctx: &CliContext,
            _base_url: &str,
        ) -> Result<Self::Outcome, Error> {
            Ok("daemon")
        }

        async fn run_embedded(
            &self,
            _ctx: &CliContext,
            _config_loader: &ConfigLoader,
            _db: &AppDb,
        ) -> Result<Self::Outcome, Error> {
            Ok("embedded")
        }
    }

    async fn test_loader_and_db(dir: &TempDir) -> (ConfigLoader, AppDb) {
        let mut defaults = DefaultsConfig::default();
        defaults.indexing.embedding = EmbeddingPolicy {
            provider: "fake".into(),
            model: "default".into(),
        };
        let config = RawConfig {
            version: 1,
            server: ServerConfig::default(),
            paths: PathsConfig::default(),
            defaults,
            providers: vec![],
        };
        let paths = ResolvedPaths {
            config_file: dir.path().join("config.yaml"),
            data_dir: dir.path().to_path_buf(),
            models_dir: dir.path().join("models"),
            logs_dir: dir.path().join("logs"),
        };
        let loader = ConfigLoader { config, paths };
        let db = crate::app_db::open_app_db_from_loader(&loader)
            .await
            .unwrap();
        (loader, db)
    }

    fn test_ctx(daemon_url: Option<&str>) -> CliContext {
        CliContext {
            config: None,
            json: false,
            stores: vec![],
            yes: false,
            daemon_url: daemon_url.map(String::from),
            config_env: None,
        }
    }

    #[tokio::test]
    async fn dispatch_routes_to_embedded_when_no_daemon_detected() {
        let dir = TempDir::new().unwrap();
        let (loader, _db) = test_loader_and_db(&dir).await;
        // No `daemon.sock`, no `LOCALDB_DAEMON_URL` override -> NotRunning.
        let ctx = test_ctx(None);
        let outcome = dispatch(&ProbeCmd, &ctx, &loader, || async {
            crate::app_db::open_app_db_from_loader(&loader)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(outcome, "embedded");
    }

    #[tokio::test]
    async fn dispatch_routes_to_daemon_when_override_present() {
        let dir = TempDir::new().unwrap();
        let (loader, _db) = test_loader_and_db(&dir).await;
        // `probe_daemon` treats a `daemon_url` override as authoritative
        // (`DaemonState::Running`) without a reachability check — see
        // `daemon_client::probe_daemon`'s doc comment — so this exercises
        // the routing decision without a real HTTP server.
        let ctx = test_ctx(Some("http://127.0.0.1:1"));
        // `open_db` panics if called at all — proves the daemon branch never
        // opens the DB (issue #187 review, finding G4): before the fix every
        // `dispatch` call site opened the local `AppDb` unconditionally, so a
        // broken local store could preempt a healthy daemon that never
        // needed it.
        let outcome = dispatch(&ProbeCmd, &ctx, &loader, || async {
            panic!("open_db must not be called when a daemon is routed to")
        })
        .await;
        assert_eq!(outcome, "daemon");
    }
}
