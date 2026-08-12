//! Pure, `Result`-returning scaffolding helpers for first-run setup.
//!
//! Factored out of `cmds/init.rs` (issue #119/#120) so the config-write and
//! default-store-creation steps can be reused by other first-run paths.
//! Nothing in this module calls `exit_err` or `std::process::exit` — every
//! failure is a plain `Result`, same discipline as
//! `app_db::resolve_store_scope_inner` and friends.
//!
//! Wired into the CLI's strict and lenient config-load paths via
//! `app_db::load_config_scaffolded` and `app_db::load_config_lenient` — every
//! command that goes through either of those (`search`, `status`, `store
//! add`/`remove`/`list`, `source add`/`list`/`remove`, `index`, `mcp`)
//! implicitly scaffolds config + a `default` store on a genuine first run,
//! same as `localdb init` does explicitly.

use std::path::{Path, PathBuf};

use localdb_core::{
    config::{
        loader::{load_config, resolve_config_path, LoadOptions},
        render_default_config_template, PlatformPaths,
    },
    ids::new_ulid,
    Error,
};

use crate::{
    app_db::{default_store_row, AppDb, DEFAULT_STORE_NAME},
    daemon_client::CliContext,
};

/// Outcome of [`ensure_config_scaffolded`].
#[derive(Debug, Clone)]
pub(crate) struct ScaffoldResult {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    pub models_dir: PathBuf,
    pub logs_dir: PathBuf,
    /// True iff *this* call wrote the config file (a genuine first run).
    /// `false` covers both "the file already existed" and "another racer
    /// won the atomic create" (see [`write_config_atomically`]).
    pub was_scaffolded: bool,
}

/// Ensure the config file (and its data/models/logs directories) exist,
/// scaffolding them from [`render_default_config_template`] if this is a
/// genuine first run.
///
/// Resolves the config path with the same 3-tier priority as
/// [`resolve_config_path`] (`--config` flag > `LOCALDB_CONFIG` env >
/// `PlatformPaths::resolve()`), then delegates the rest to
/// [`ensure_config_scaffolded_inner`] — see that function's doc comment for
/// the full absent/exists decision tree. Split so tests can supply a
/// synthetic [`PlatformPaths`] rooted in a tempdir instead of this
/// function's real, machine-global default (mirrors the `_inner` split used
/// throughout `app_db.rs`, e.g. `resolve_store_scope`/
/// `resolve_store_scope_inner`).
pub(crate) async fn ensure_config_scaffolded(ctx: &CliContext) -> Result<ScaffoldResult, Error> {
    let platform = PlatformPaths::resolve().ok_or_else(|| Error::InvalidConfig {
        message: "cannot determine platform paths (no home directory?)".to_string(),
    })?;
    ensure_config_scaffolded_inner(ctx, &platform)
}

/// The pure decision logic behind [`ensure_config_scaffolded`]; see that
/// function's doc comment for why `platform` is a parameter rather than a
/// `PlatformPaths::resolve()` call made here.
///
/// - If the resolved config path **exists** (even malformed): returns
///   `was_scaffolded: false` immediately. Existence is checked with
///   `Path::exists`, never by attempting a parse-then-fail — the caller's
///   own strict load is what hard-fails on malformed content, per this
///   function's contract. Directories are resolved from the existing file's
///   `paths.*` when it happens to load cleanly (a free bonus from calling
///   `load_config`), else from `platform`'s defaults; either way, nothing is
///   created on disk in this branch.
/// - If the resolved config path is **absent**: directories are `platform`'s
///   defaults, `create_dir_all`'d (config parent + data + models + logs),
///   then [`render_default_config_template`]'s content is written via
///   [`write_config_atomically`] and `was_scaffolded: true` is returned.
///
/// The F11 guard (mirrors `cmds/init.rs`'s `run_init_async`, ~:55-69) runs
/// first: an *explicit* `--config` whose parent directory doesn't exist is
/// `Error::InvalidConfig`, not a silent fall-through to platform defaults —
/// preserved here so lazy-init call sites get the same exit-2 behavior
/// `init` already gives a typo'd `--config` path.
fn ensure_config_scaffolded_inner(
    ctx: &CliContext,
    platform: &PlatformPaths,
) -> Result<ScaffoldResult, Error> {
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_path = resolve_config_path(&options, ctx.config_env.as_deref())?;

    if ctx.config.is_some() {
        if let Some(parent) = config_path.parent() {
            if !parent.exists() && parent != Path::new("") {
                return Err(Error::InvalidConfig {
                    message: format!(
                        "config path parent directory '{}' does not exist",
                        parent.display()
                    ),
                });
            }
        }
    }

    if config_path.exists() {
        let (data_dir, models_dir, logs_dir) =
            match load_config(&options, ctx.config_env.as_deref()) {
                Ok(loaded) => (
                    loaded.paths.data_dir,
                    loaded.paths.models_dir,
                    loaded.paths.logs_dir,
                ),
                Err(_) => (
                    platform.data_dir.clone(),
                    platform.models_dir.clone(),
                    platform.logs_dir.clone(),
                ),
            };
        return Ok(ScaffoldResult {
            config_path,
            data_dir,
            models_dir,
            logs_dir,
            was_scaffolded: false,
        });
    }

    let data_dir = platform.data_dir.clone();
    let models_dir = platform.models_dir.clone();
    let logs_dir = platform.logs_dir.clone();

    for dir in [
        config_path.parent().unwrap_or_else(|| Path::new(".")),
        data_dir.as_path(),
        models_dir.as_path(),
        logs_dir.as_path(),
    ] {
        std::fs::create_dir_all(dir).map_err(|e| Error::Internal {
            message: format!("cannot create directory '{}': {}", dir.display(), e),
            correlation_id: "scaffold_config_mkdir".to_string(),
        })?;
    }

    write_config_atomically(&config_path, &render_default_config_template())?;

    Ok(ScaffoldResult {
        config_path,
        data_dir,
        models_dir,
        logs_dir,
        was_scaffolded: true,
    })
}

/// Emit a single observability line for a genuine first-run scaffold.
///
/// Called by both `app_db::load_config_scaffolded` and
/// `app_db::load_config_lenient` right after a `was_scaffolded: true`
/// result — the only place either needs `ScaffoldResult`'s path fields for
/// anything beyond the tests below (which construct and inspect them
/// directly).
pub(crate) fn log_scaffold_result(result: &ScaffoldResult) {
    tracing::info!(
        config_path = %result.config_path.display(),
        data_dir = %result.data_dir.display(),
        models_dir = %result.models_dir.display(),
        logs_dir = %result.logs_dir.display(),
        "scaffolded default localdb config on first run"
    );
}

/// Write `content` to `config_path` such that any concurrent reader (in
/// particular the daemon's config file watcher,
/// `server/src/daemon.rs::run_config_watcher`) never observes partial
/// content.
///
/// Writes the full content to a uniquely-named temp file
/// (`config.yaml.tmp-<pid>-<ulid>`) in the *same* directory as `config_path`
/// — same directory so the final `hard_link` is same-filesystem — then
/// `hard_link`s it into place. `hard_link` either creates the destination
/// atomically or fails with `AlreadyExists`; a concurrent racer winning that
/// race is treated as success, not an error, since both writers were writing
/// the same template content. The temp file is removed in every case
/// (success, lost race, or hard failure) so a failed attempt never leaves a
/// stray `config.yaml.tmp-*` behind.
fn write_config_atomically(config_path: &Path, content: &str) -> Result<(), Error> {
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let tmp_path = parent.join(format!(
        "config.yaml.tmp-{}-{}",
        std::process::id(),
        new_ulid()
    ));

    let result = std::fs::write(&tmp_path, content)
        .map_err(|e| Error::Internal {
            message: format!(
                "cannot write temp config file '{}': {}",
                tmp_path.display(),
                e
            ),
            correlation_id: "scaffold_config_tmp_write".to_string(),
        })
        .and_then(|()| match std::fs::hard_link(&tmp_path, config_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(Error::Internal {
                message: format!("cannot link config file '{}': {}", config_path.display(), e),
                correlation_id: "scaffold_config_link".to_string(),
            }),
        });

    let _ = std::fs::remove_file(&tmp_path);

    result
}

/// Ensure a store named `"default"` exists, creating it if absent.
///
/// Looks up the store by name and inserts it via the same
/// `store_factory::default_store_row` + `upsert_store` path `cmds/init.rs`'s
/// `run_init_async` uses (~:129-141). If the insert itself errors, this
/// re-checks by name and treats the store's *existence* — not the error's
/// shape — as the source of truth for success: `upsert_store` conflicts on
/// `id` only (`INSERT ... ON CONFLICT(id) DO UPDATE`), while `stores.name` is
/// UNIQUE at the schema level, so a name collision from a concurrent racer
/// surfaces as a generic `Error::Internal`
/// (store-libsql/src/registry/stores.rs:14-20, schema.rs:38,
/// connection.rs:344-353) rather than a distinguishable "already exists"
/// variant. Pattern-matching the error kind would therefore be wrong; the
/// recheck-by-name is the robust rule.
pub(crate) async fn ensure_default_store(db: &AppDb) -> Result<(), Error> {
    if db
        .backend()
        .get_store_by_name(DEFAULT_STORE_NAME)
        .await?
        .is_some()
    {
        return Ok(());
    }

    let default_store = default_store_row(DEFAULT_STORE_NAME, db)?;
    if let Err(upsert_err) = db.backend().upsert_store(&default_store).await {
        return match db.backend().get_store_by_name(DEFAULT_STORE_NAME).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(upsert_err),
            Err(_) => Err(upsert_err),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::config::loader::ResolvedPaths;
    use localdb_core::config::schema::{
        DefaultsConfig, EmbeddingPolicy, PathsConfig, RawConfig, ServerConfig,
    };
    use tempfile::TempDir;

    fn test_ctx(config: Option<PathBuf>) -> CliContext {
        CliContext {
            config,
            json: false,
            stores: vec![],
            yes: false,
            daemon_url: None,
            config_env: None,
        }
    }

    /// A `PlatformPaths` rooted entirely inside `dir`, so tests never touch
    /// the real machine's default config/data/models/logs directories.
    fn synthetic_platform(dir: &TempDir) -> PlatformPaths {
        PlatformPaths {
            config_file: dir.path().join("platform-default").join("config.yaml"),
            data_dir: dir.path().join("data"),
            models_dir: dir.path().join("models"),
            logs_dir: dir.path().join("logs"),
        }
    }

    async fn tmp_app_db(dir: &TempDir) -> AppDb {
        let mut defaults = DefaultsConfig::default();
        defaults.indexing.embedding = EmbeddingPolicy {
            provider: "fake".into(),
            model: "default".into(),
        };
        let config = RawConfig {
            version: 1,
            schema: None,
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
        AppDb::open(
            &paths,
            &config.defaults.indexing.embedding,
            &config.providers,
            config.defaults.indexing.clone(),
        )
        .await
        .unwrap()
    }

    // --- ensure_config_scaffolded ---

    #[test]
    fn ensure_config_scaffolded_writes_template_when_absent() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("cfg").join("config.yaml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let ctx = test_ctx(Some(config_path.clone()));
        let platform = synthetic_platform(&dir);

        let result = ensure_config_scaffolded_inner(&ctx, &platform).unwrap();

        assert!(result.was_scaffolded);
        assert_eq!(result.config_path, config_path);
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, render_default_config_template());
        assert!(result.data_dir.is_dir());
        assert!(result.models_dir.is_dir());
        assert!(result.logs_dir.is_dir());
    }

    #[test]
    fn ensure_config_scaffolded_is_noop_when_config_exists() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        let arbitrary: &[u8] = b"this is not a valid localdb config at all\n%%%\n";
        std::fs::write(&config_path, arbitrary).unwrap();
        let ctx = test_ctx(Some(config_path.clone()));
        let platform = synthetic_platform(&dir);

        let result = ensure_config_scaffolded_inner(&ctx, &platform).unwrap();

        assert!(!result.was_scaffolded);
        let after = std::fs::read(&config_path).unwrap();
        assert_eq!(
            after, arbitrary,
            "existing (even malformed) config bytes must be untouched"
        );
    }

    #[test]
    fn ensure_config_scaffolded_explicit_config_missing_parent_is_invalid_config() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("missing-parent").join("config.yaml");
        let ctx = test_ctx(Some(config_path));
        let platform = synthetic_platform(&dir);

        let err = ensure_config_scaffolded_inner(&ctx, &platform).unwrap_err();
        assert!(
            matches!(err, Error::InvalidConfig { .. }),
            "expected InvalidConfig, got {err:?}"
        );
    }

    #[test]
    fn ensure_config_scaffolded_creates_data_models_logs_dirs() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        let ctx = test_ctx(Some(config_path));
        let platform = synthetic_platform(&dir);
        assert!(!platform.data_dir.exists());
        assert!(!platform.models_dir.exists());
        assert!(!platform.logs_dir.exists());

        let result = ensure_config_scaffolded_inner(&ctx, &platform).unwrap();

        assert_eq!(result.data_dir, platform.data_dir);
        assert_eq!(result.models_dir, platform.models_dir);
        assert_eq!(result.logs_dir, platform.logs_dir);
        assert!(platform.data_dir.is_dir());
        assert!(platform.models_dir.is_dir());
        assert!(platform.logs_dir.is_dir());
    }

    #[test]
    fn ensure_config_scaffolded_concurrent_calls_all_succeed() {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        let platform = synthetic_platform(&dir);

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let ctx = test_ctx(Some(config_path.clone()));
                let platform = platform.clone();
                std::thread::spawn(move || ensure_config_scaffolded_inner(&ctx, &platform))
            })
            .collect();

        for h in handles {
            h.join()
                .expect("thread must not panic")
                .expect("every concurrent call must succeed");
        }

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(content, render_default_config_template());
        // No stray temp files left behind by any racer.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("config.yaml.tmp-")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "expected no leftover temp files, found {leftovers:?}"
        );
    }

    // --- ensure_default_store ---

    #[tokio::test]
    async fn ensure_default_store_creates_when_absent() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        assert!(db
            .backend()
            .get_store_by_name(DEFAULT_STORE_NAME)
            .await
            .unwrap()
            .is_none());

        ensure_default_store(&db).await.unwrap();

        assert!(db
            .backend()
            .get_store_by_name(DEFAULT_STORE_NAME)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn ensure_default_store_is_noop_when_present() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let row = default_store_row(DEFAULT_STORE_NAME, &db).unwrap();
        db.backend().upsert_store(&row).await.unwrap();

        ensure_default_store(&db).await.unwrap();

        let stores = db.backend().list_stores().await.unwrap();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].id, row.id);
    }

    #[tokio::test]
    async fn ensure_default_store_concurrent_calls_do_not_error_or_duplicate() {
        let dir = TempDir::new().unwrap();
        let db = std::sync::Arc::new(tmp_app_db(&dir).await);

        let mut handles = Vec::new();
        for _ in 0..8 {
            let db = db.clone();
            handles.push(tokio::spawn(async move { ensure_default_store(&db).await }));
        }
        for h in handles {
            h.await
                .expect("task must not panic")
                .expect("every concurrent call must succeed");
        }

        let stores = db.backend().list_stores().await.unwrap();
        let default_count = stores
            .iter()
            .filter(|s| s.name == DEFAULT_STORE_NAME)
            .count();
        assert_eq!(default_count, 1, "expected exactly one default store");
    }
}
