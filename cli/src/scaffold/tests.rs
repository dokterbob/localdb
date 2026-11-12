use super::*;
use localdb_core::config::loader::ResolvedPaths;
use localdb_core::config::schema::{DefaultsConfig, EmbeddingPolicy, RawConfig};
use tempfile::TempDir;

fn test_ctx(config: Option<PathBuf>) -> CliContext {
    CliContext {
        config,
        json: false,
        stores: vec![],
        yes: false,
        daemon_url: None,
        config_env: None,
        api_key: None,
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
        defaults,
        ..Default::default()
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

/// Scaffolding I/O failures are `invalid_config`/exit 2 per
/// specs/05-surfaces.md §2.5, same as the F11 missing-parent guard —
/// not `internal`/exit 1. The data dir's parent is a regular *file*, so
/// `create_dir_all` fails with `NotADirectory` (portable across the unix
/// platforms CI runs on).
#[test]
fn ensure_config_scaffolded_io_failure_is_invalid_config() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    let ctx = test_ctx(Some(config_path));
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"i am a file, not a directory").unwrap();
    let mut platform = synthetic_platform(&dir);
    platform.data_dir = blocker.join("data");

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

// --- config_is_pristine_template ---

#[test]
fn config_is_pristine_template_true_for_scaffolded_false_once_edited() {
    let dir = TempDir::new().unwrap();
    let config_path = dir.path().join("config.yaml");
    assert!(
        !config_is_pristine_template(&config_path),
        "an absent file is not the pristine template"
    );

    std::fs::write(&config_path, render_default_config_template()).unwrap();
    assert!(config_is_pristine_template(&config_path));

    let mut edited = render_default_config_template();
    edited.push_str("\n# user note\n");
    std::fs::write(&config_path, edited).unwrap();
    assert!(
        !config_is_pristine_template(&config_path),
        "any edit must opt the config out of pristine-template seeding"
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
