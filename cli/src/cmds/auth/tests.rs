use super::*;
use localdb_core::auth::{AuthStore, TOKEN_PREFIX};
use localdb_core::config::loader::ResolvedPaths;
use localdb_core::config::schema::{EmbeddingPolicy, IndexingPolicyConfig};
use tempfile::TempDir;

async fn tmp_app_db(dir: &TempDir) -> AppDb {
    let indexing = IndexingPolicyConfig {
        embedding: EmbeddingPolicy {
            provider: "fake".into(),
            model: "default".into(),
        },
        ..Default::default()
    };
    let paths = ResolvedPaths {
        config_file: dir.path().join("config.yaml"),
        data_dir: dir.path().to_path_buf(),
        models_dir: dir.path().join("models"),
        logs_dir: dir.path().join("logs"),
    };
    AppDb::open(&paths, &indexing.embedding.clone(), &[], indexing)
        .await
        .unwrap()
}

#[tokio::test]
async fn issue_key_for_existing_user_mints_show_once_secret() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    db.auth_service()
        .create_user("alice", Role::Admin)
        .await
        .unwrap();

    let issued = issue_key_for_user(&db, "alice").await.unwrap();

    assert!(issued.secret.starts_with(TOKEN_PREFIX));
    // The minted key authenticates against the same database.
    let principal = db
        .auth_service()
        .authenticate(&issued.secret)
        .await
        .unwrap();
    assert_eq!(principal.name, "alice");
    assert_eq!(principal.role, Role::Admin);
}

#[tokio::test]
async fn issue_key_for_unknown_user_is_invalid_request() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;

    let err = issue_key_for_user(&db, "nobody").await.unwrap_err();

    assert!(
        matches!(err, Error::InvalidRequest { ref message } if message.contains("nobody")),
        "expected InvalidRequest naming the user, got: {err:?}"
    );
}

#[tokio::test]
async fn app_db_auth_tables_persist_across_reopen() {
    // The persistence property the daemon relies on: a user created via
    // one AppDb handle (break-glass CLI) is visible when the same
    // database file is opened again (a subsequently started daemon).
    let dir = TempDir::new().unwrap();
    {
        let db = tmp_app_db(&dir).await;
        db.auth_service()
            .create_user("bob", Role::Member)
            .await
            .unwrap();
    }
    let db2 = tmp_app_db(&dir).await;
    let user = db2
        .auth_store()
        .get_user_by_name("bob")
        .await
        .unwrap()
        .expect("user must survive reopen of the on-disk database");
    assert_eq!(user.role, Role::Member);
}
