use super::*;
use localdb_core::config::schema::{DefaultsConfig, RawConfig};
use localdb_core::{ids::new_ulid, ingestion::now_rfc3339, types::SourceKind, SourceRow};
use tempfile::TempDir;

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

fn test_store_row(name: &str, db: &AppDb) -> StoreRow {
    default_store_row(name, db).unwrap()
}

fn test_source_row(store_id: &str, root: &str) -> SourceRow {
    SourceRow {
        id: new_ulid(),
        store_id: store_id.to_string(),
        kind: SourceKind::Path,
        root: Some(root.to_string()),
        url: None,
        include: vec![],
        exclude: vec![],
        preset: "prose".to_string(),
        refresh: None,
        created_at: now_rfc3339(),
        config_json: None,
        feed_etag: None,
        feed_last_modified: None,
        feed_inputs_digest: None,
    }
}

#[tokio::test]
async fn app_db_store_add_list_remove() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    assert!(db.backend().list_stores().await.unwrap().is_empty());
    let store = test_store_row("mystore", &db);
    let id = store.id.clone();
    db.backend().upsert_store(&store).await.unwrap();
    let stores = db.backend().list_stores().await.unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].name, "mystore");
    assert!(db.backend().delete_store(&id).await.unwrap());
}

// -----------------------------------------------------------------------
// AppDbStoreProvider — T2 realtime MCP store resolution
// -----------------------------------------------------------------------

#[tokio::test]
async fn app_db_store_provider_reflects_stores_added_after_construction() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(tmp_app_db(&dir).await);
    let provider = AppDbStoreProvider::new(db.clone(), vec![]);

    let before = provider.available_stores().await.unwrap();
    assert!(before.is_empty(), "no stores yet");

    let store = default_store_row("late-store", &db).unwrap();
    db.backend().upsert_store(&store).await.unwrap();

    let after = provider.available_stores().await.unwrap();
    assert_eq!(after.len(), 1, "the newly added store must be visible");
    assert_eq!(after[0].descriptor.name, "late-store");
}

#[tokio::test]
async fn app_db_store_provider_narrows_by_store_names() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(tmp_app_db(&dir).await);
    for name in ["alpha", "beta"] {
        let store = default_store_row(name, &db).unwrap();
        db.backend().upsert_store(&store).await.unwrap();
    }

    let provider = AppDbStoreProvider::new(db.clone(), vec!["beta".to_string()]);
    let stores = provider.available_stores().await.unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].descriptor.name, "beta");
}
fn test_ctx(stores: Vec<&str>) -> CliContext {
    CliContext {
        config: None,
        json: false,
        stores: stores.into_iter().map(String::from).collect(),
        yes: false,
        daemon_url: None,
        config_env: None,
        api_key: None,
    }
}

#[test]
fn reject_store_flag_inner_with_store_errors() {
    let ctx = test_ctx(vec!["a"]);
    let err = reject_store_flag_inner(&ctx, DB_REJECT_MESSAGE).unwrap_err();
    assert_eq!(
        err,
        Error::InvalidRequest {
            message: "`db` commands operate on the whole database file; --store is not applicable"
                .to_string(),
        }
    );
}

/// The caller's `message` is the entire user-visible error text — the
/// helper never prefixes or rewrites it, which is what lets one function
/// serve `db`, `store add`/`remove`, `init` and `serve` with four
/// different explanations.
#[test]
fn reject_store_flag_inner_uses_the_callers_message_verbatim() {
    let ctx = test_ctx(vec!["a"]);
    let err = reject_store_flag_inner(&ctx, "totally bespoke explanation").unwrap_err();
    assert_eq!(
        err,
        Error::InvalidRequest {
            message: "totally bespoke explanation".to_string(),
        }
    );
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn reject_store_flag_inner_without_store_is_ok() {
    let ctx = test_ctx(vec![]);
    assert!(reject_store_flag_inner(&ctx, DB_REJECT_MESSAGE).is_ok());
}

/// `AllStoresAllowEmpty` is the one all-stores policy that resolves a
/// zero-store database to an empty scope instead of exit 2 — the
/// difference `search`/`mcp` depend on (specs/05-surfaces.md §2.2).
#[tokio::test]
async fn scope_all_stores_allow_empty_resolves_empty_scope() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let ctx = test_ctx(vec![]);
    let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStoresAllowEmpty)
        .await
        .expect("an empty database must resolve, not error, under AllStoresAllowEmpty");
    assert!(rows.is_empty());
}

/// `AllStoresAllowEmpty` differs from `AllStores` *only* in the
/// empty-database case: with stores present it still spans all of them.
#[tokio::test]
async fn scope_all_stores_allow_empty_still_spans_every_store() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    for name in ["a", "b"] {
        let row = test_store_row(name, &db);
        db.backend().upsert_store(&row).await.unwrap();
    }
    let ctx = test_ctx(vec![]);
    let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStoresAllowEmpty)
        .await
        .unwrap();
    let names: std::collections::HashSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["a", "b"].into_iter().collect());
}

/// An explicit unknown `-s` is still exit 3 under `AllStoresAllowEmpty` —
/// "allow empty" relaxes only the *omitted*-`-s` case, never validation.
#[tokio::test]
async fn scope_all_stores_allow_empty_still_rejects_unknown_explicit_name() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let ctx = test_ctx(vec!["nope"]);
    let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStoresAllowEmpty)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::StoreNotFound {
            id: "nope".to_string()
        }
    );
    assert_eq!(err.exit_code(), 3);
}

#[tokio::test]
async fn scope_explicit_names_resolved_in_order_and_deduped() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let a = test_store_row("a", &db);
    let b = test_store_row("b", &db);
    db.backend().upsert_store(&a).await.unwrap();
    db.backend().upsert_store(&b).await.unwrap();

    let ctx = test_ctx(vec!["a", "b", "a"]);
    let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "a");
    assert_eq!(rows[1].name, "b");
}

#[tokio::test]
async fn scope_explicit_unknown_name_errors_store_not_found() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let ctx = test_ctx(vec!["nope"]);
    let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::StoreNotFound {
            id: "nope".to_string()
        }
    );
}

#[tokio::test]
async fn scope_explicit_traversal_name_rejected_by_validate_store_name() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let ctx = test_ctx(vec!["../evil"]);
    let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
}

#[tokio::test]
async fn scope_all_stores_empty_errors_no_stores() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let ctx = test_ctx(vec![]);
    let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::AllStores)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::InvalidRequest {
            message: "no stores; run `localdb store add <name>` or pass --store".to_string(),
        }
    );
}

#[tokio::test]
async fn scope_default_store_missing_with_other_store_present_errors() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let other = test_store_row("other", &db);
    db.backend().upsert_store(&other).await.unwrap();

    let ctx = test_ctx(vec![]);
    let err = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::DefaultStore)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        Error::InvalidRequest {
            message: "no store named 'default'; pass --store <name>".to_string(),
        }
    );
}

#[tokio::test]
async fn scope_default_store_present_returns_it() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let default_row = test_store_row(DEFAULT_STORE_NAME, &db);
    db.backend().upsert_store(&default_row).await.unwrap();

    let ctx = test_ctx(vec![]);
    let rows = resolve_store_scope_inner(&ctx, &db, StoreScopePolicy::DefaultStore)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, DEFAULT_STORE_NAME);
}

/// Finding 5 (Codex review): an invalid `--store` name must be rejected
/// as `Error::InvalidRequest` (exit 2) *before* the daemon store list is
/// fetched — the daemon base URL here (`127.0.0.1:0`) is guaranteed
/// connection-refused (see `daemon_client::tests::probe_stale_removes_both_socket_and_url_file`
/// for the same idiom), so if validation ran after the fetch this would
/// surface `Error::DaemonUnreachable` (exit 5) instead — exactly the
/// ordering bug the function's doc comment already promised was fixed.
#[tokio::test]
async fn resolve_daemon_store_scope_inner_validates_before_fetching() {
    let ctx = test_ctx(vec!["../bad"]);
    let err =
        resolve_daemon_store_scope_inner("http://127.0.0.1:0", &ctx, StoreScopePolicy::AllStores)
            .await
            .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(
        matches!(err, Error::InvalidRequest { .. }),
        "expected InvalidRequest, got {err:?}"
    );
}

/// Pin the empty-`--store` (no flags passed) daemon-scope behavior: with
/// nothing to validate, the call proceeds straight to the daemon fetch,
/// so an unreachable daemon still surfaces as `DaemonUnreachable` (exit
/// 5) rather than being reinterpreted as a validation error.
#[tokio::test]
async fn resolve_daemon_store_scope_inner_empty_stores_still_reaches_daemon() {
    let ctx = test_ctx(vec![]);
    let err =
        resolve_daemon_store_scope_inner("http://127.0.0.1:0", &ctx, StoreScopePolicy::AllStores)
            .await
            .unwrap_err();
    assert_eq!(err, Error::DaemonUnreachable);
    assert_eq!(err.exit_code(), 5);
}

#[tokio::test]
async fn app_db_source_upsert_list_delete() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let store = test_store_row("s1", &db);
    db.backend().upsert_store(&store).await.unwrap();
    let store_id = db.resolve_store_id("s1").await.unwrap();
    let src = test_source_row(&store_id, "/tmp");
    db.backend().upsert_source(&src).await.unwrap();
    let list = db.backend().list_sources(&store_id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, src.id);
    assert!(db.backend().delete_source(&src.id).await.unwrap());
}
