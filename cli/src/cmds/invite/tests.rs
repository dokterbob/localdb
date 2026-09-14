use super::*;
use crate::app_db::AppDb;
use localdb_core::auth::Role;
use localdb_core::config::loader::ResolvedPaths;
use localdb_core::config::schema::{EmbeddingPolicy, IndexingPolicyConfig};
use localdb_core::types::StoreVisibility;
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

#[test]
fn parse_expiry_duration_supports_days() {
    assert_eq!(parse_expiry_duration("7d").unwrap(), 7 * 86400);
}

#[test]
fn parse_expiry_duration_supports_hours_minutes_seconds() {
    assert_eq!(parse_expiry_duration("24h").unwrap(), 86400);
    assert_eq!(parse_expiry_duration("30m").unwrap(), 1800);
    assert_eq!(parse_expiry_duration("3600s").unwrap(), 3600);
    assert_eq!(parse_expiry_duration("60").unwrap(), 60);
}

#[test]
fn parse_expiry_duration_rejects_zero_and_garbage() {
    assert!(parse_expiry_duration("0d").is_err());
    assert!(parse_expiry_duration("nonsense").is_err());
    assert!(parse_expiry_duration("").is_err());
}

#[tokio::test]
async fn create_invite_direct_db_then_list_and_revoke() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    db.auth_service()
        .create_user("admin", Role::Admin)
        .await
        .unwrap();

    let issued = db
        .auth_service()
        .create_invite(InviteMode::Open, &[], 1, None, "admin")
        .await
        .unwrap();
    assert!(issued.secret.starts_with("ldb_"));

    let all = db.auth_store().list_invites().await.unwrap();
    assert_eq!(all.len(), 1);

    assert!(db.auth_store().revoke_invite(&issued.row.id).await.unwrap());
}

#[tokio::test]
async fn create_invite_with_store_grant_resolves_visibility_direct_db() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let store = localdb_core::StoreRow {
        id: "store-docs".to_string(),
        name: "docs".to_string(),
        visibility: StoreVisibility::Shared,
        backend: "libsql".to_string(),
        indexing_policy: "{}".to_string(),
        policy_version: "v1".to_string(),
        created_at: localdb_core::ingestion::now_rfc3339(),
    };
    db.backend().upsert_store(&store).await.unwrap();

    let issued = db
        .auth_service()
        .create_invite(
            InviteMode::Open,
            &[("docs".to_string(), StoreVisibility::Shared)],
            1,
            None,
            "admin",
        )
        .await
        .unwrap();
    assert_eq!(issued.row.store_grants, vec!["docs".to_string()]);
}

#[tokio::test]
async fn approve_and_deny_direct_db_round_trip() {
    let dir = TempDir::new().unwrap();
    let db = tmp_app_db(&dir).await;
    let issued = db
        .auth_service()
        .create_invite(InviteMode::Closed, &[], 2, None, "admin")
        .await
        .unwrap();

    let outcome = db
        .auth_service()
        .redeem_invite(&issued.secret, "alice")
        .await
        .unwrap();
    let localdb_core::auth::RedeemOutcome::Closed { request_id, .. } = outcome else {
        panic!("expected Closed outcome");
    };
    let user = db
        .auth_service()
        .approve_request(&request_id)
        .await
        .unwrap();
    assert_eq!(user.name, "alice");

    let outcome2 = db
        .auth_service()
        .redeem_invite(&issued.secret, "bob")
        .await
        .unwrap();
    let localdb_core::auth::RedeemOutcome::Closed {
        request_id: request_id2,
        ..
    } = outcome2
    else {
        panic!("expected Closed outcome");
    };
    db.auth_service().deny_request(&request_id2).await.unwrap();

    let requests = db.auth_store().list_access_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
}
