use super::*;
use localdb_core::config::loader::ResolvedPaths;
use localdb_core::config::schema::{DefaultsConfig, EmbeddingPolicy, RawConfig};
use tempfile::TempDir;

/// A minimal `DaemonAwareCommand` whose two branches return distinct,
/// directly-observable outcomes — enough to prove `dispatch` routed to
/// the right one without needing a real HTTP daemon.
struct ProbeCmd;

impl DaemonAwareCommand for ProbeCmd {
    type Outcome = &'static str;
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStoresAllowEmpty;

    async fn run_daemon(&self, _ctx: &CliContext, _base_url: &str) -> Result<Self::Outcome, Error> {
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
        defaults,
        ..Default::default()
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
        api_key: None,
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
