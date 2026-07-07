//! Integration tests for `LibsqlAuthStore` against a real tmpdir libsql
//! database — every `AuthStore` mutation path is exercised here (coverage
//! gate: data-modifying paths >= 90%, specs/01-architecture.md §7).

use localdb_core::auth::{
    AccessRequestRow, AccessRequestState, AuthStore, AuthTokenRow, InviteMode, InviteRow, Role,
    StoreGrantRow, TokenKind, UserRow,
};
use localdb_core::types::StoreVisibility;
use localdb_core::{Error, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
use tempfile::tempdir;

use super::LibsqlAuthStore;
use crate::SqliteBackend;

/// Build an `AuthStore` via the same public path a real caller uses
/// (`SqliteBackend::auth_store()`), exercising that accessor too rather than
/// reaching into `crate::connection::LibsqlDb` directly. Also returns the
/// backing `SqliteBackend` so grant tests can insert a real `stores` row —
/// `store_grants.store_name` has a `REFERENCES stores(name)` FK.
async fn make_store() -> (tempfile::TempDir, SqliteBackend, LibsqlAuthStore) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        4,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    let auth_store = backend.auth_store();
    (dir, backend, auth_store)
}

/// Insert a real `stores` row named `name` — required before granting
/// access to it (`store_grants.store_name` FK-references `stores(name)`).
async fn insert_store_row(backend: &SqliteBackend, name: &str) {
    backend
        .upsert_store(&StoreRow {
            id: format!("store-{name}"),
            name: name.to_string(),
            visibility: StoreVisibility::Shared,
            backend: "libsql".to_string(),
            indexing_policy: "{}".to_string(),
            policy_version: "v1".to_string(),
            acl: "{}".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();
}

fn make_user(id: &str, name: &str, role: Role) -> UserRow {
    UserRow {
        id: id.to_string(),
        name: name.to_string(),
        role,
        created_at: "2026-06-10T12:00:00Z".to_string(),
    }
}

fn make_token(id: &str, user_id: &str, kind: TokenKind, secret_hash: &str) -> AuthTokenRow {
    AuthTokenRow {
        id: id.to_string(),
        user_id: user_id.to_string(),
        kind,
        secret_hash: secret_hash.to_string(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        family_id: None,
        rotated_from: None,
    }
}

// ---------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------

#[tokio::test]
async fn create_and_get_user_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    let user = make_user("u1", "alice", Role::Admin);
    store.create_user(&user).await.unwrap();

    let found = store.get_user("u1").await.unwrap().unwrap();
    assert_eq!(found, user);

    let found_by_name = store.get_user_by_name("alice").await.unwrap().unwrap();
    assert_eq!(found_by_name, user);
}

#[tokio::test]
async fn get_user_missing_returns_none() {
    let (_dir, _backend, store) = make_store().await;
    assert!(store.get_user("nope").await.unwrap().is_none());
    assert!(store.get_user_by_name("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn create_user_rejects_duplicate_name() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let err = store
        .create_user(&make_user("u2", "alice", Role::Member))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn list_users_returns_all() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "bob", Role::Member))
        .await
        .unwrap();
    let all = store.list_users().await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test]
async fn update_user_role_changes_role() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Member))
        .await
        .unwrap();
    store.update_user_role("u1", Role::Admin).await.unwrap();
    let found = store.get_user("u1").await.unwrap().unwrap();
    assert_eq!(found.role, Role::Admin);
}

#[tokio::test]
async fn update_user_role_errors_when_user_missing() {
    let (_dir, _backend, store) = make_store().await;
    let err = store
        .update_user_role("nope", Role::Admin)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn delete_user_removes_row_and_reports_result() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    assert!(store.delete_user("u1").await.unwrap());
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(!store.delete_user("u1").await.unwrap());
}

#[tokio::test]
async fn count_users_tracks_inserts_and_deletes() {
    let (_dir, _backend, store) = make_store().await;
    assert_eq!(store.count_users().await.unwrap(), 0);
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    assert_eq!(store.count_users().await.unwrap(), 1);
    store.delete_user("u1").await.unwrap();
    assert_eq!(store.count_users().await.unwrap(), 0);
}

// ---------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------

#[tokio::test]
async fn insert_and_find_token_by_hash_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let token = make_token("t1", "u1", TokenKind::ApiKey, "hash-1");
    store.insert_token(&token).await.unwrap();

    let found = store.find_token_by_hash("hash-1").await.unwrap().unwrap();
    assert_eq!(found, token);
    assert!(store.find_token_by_hash("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn revoke_token_sets_revoked_at_once() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let token = make_token("t1", "u1", TokenKind::ApiKey, "hash-1");
    store.insert_token(&token).await.unwrap();

    assert!(store.revoke_token("t1").await.unwrap());
    let found = store.find_token_by_hash("hash-1").await.unwrap().unwrap();
    assert!(found.revoked_at.is_some());

    // Revoking an already-revoked token reports no-op (false).
    assert!(!store.revoke_token("t1").await.unwrap());
}

#[tokio::test]
async fn revoke_token_missing_returns_false() {
    let (_dir, _backend, store) = make_store().await;
    assert!(!store.revoke_token("nope").await.unwrap());
}

#[tokio::test]
async fn revoke_token_family_revokes_all_members_only() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();

    let mut a = make_token("t1", "u1", TokenKind::Refresh, "hash-a");
    a.family_id = Some("fam-1".to_string());
    let mut b = make_token("t2", "u1", TokenKind::Refresh, "hash-b");
    b.family_id = Some("fam-1".to_string());
    let mut other = make_token("t3", "u1", TokenKind::Refresh, "hash-c");
    other.family_id = Some("fam-2".to_string());

    store.insert_token(&a).await.unwrap();
    store.insert_token(&b).await.unwrap();
    store.insert_token(&other).await.unwrap();

    let revoked_count = store.revoke_token_family("fam-1").await.unwrap();
    assert_eq!(revoked_count, 2);

    assert!(store
        .find_token_by_hash("hash-a")
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(store
        .find_token_by_hash("hash-b")
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(
        store
            .find_token_by_hash("hash-c")
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_none(),
        "a different family must not be touched"
    );
}

#[tokio::test]
async fn mark_token_used_sets_last_used_at() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    let token = make_token("t1", "u1", TokenKind::ApiKey, "hash-1");
    store.insert_token(&token).await.unwrap();

    store
        .mark_token_used("t1", "2026-06-11T00:00:00Z")
        .await
        .unwrap();
    let found = store.find_token_by_hash("hash-1").await.unwrap().unwrap();
    assert_eq!(found.last_used_at.as_deref(), Some("2026-06-11T00:00:00Z"));
}

#[tokio::test]
async fn list_tokens_for_user_filters_correctly() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("u1", "alice", Role::Admin))
        .await
        .unwrap();
    store
        .create_user(&make_user("u2", "bob", Role::Member))
        .await
        .unwrap();
    store
        .insert_token(&make_token("t1", "u1", TokenKind::ApiKey, "hash-1"))
        .await
        .unwrap();
    store
        .insert_token(&make_token("t2", "u1", TokenKind::Access, "hash-2"))
        .await
        .unwrap();
    store
        .insert_token(&make_token("t3", "u2", TokenKind::ApiKey, "hash-3"))
        .await
        .unwrap();

    let u1_tokens = store.list_tokens_for_user("u1").await.unwrap();
    assert_eq!(u1_tokens.len(), 2);
    let u2_tokens = store.list_tokens_for_user("u2").await.unwrap();
    assert_eq!(u2_tokens.len(), 1);
}

// ---------------------------------------------------------------------
// Grants
// ---------------------------------------------------------------------

#[tokio::test]
async fn grant_store_then_list_for_user_and_store() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    let grant = StoreGrantRow {
        store_name: "docs".to_string(),
        user_id: "u1".to_string(),
        granted_by: "admin-1".to_string(),
        created_at: "2026-06-10T12:00:00Z".to_string(),
    };
    store.grant_store(&grant).await.unwrap();

    let for_user = store.list_grants_for_user("u1").await.unwrap();
    assert_eq!(for_user, vec![grant.clone()]);

    let for_store = store.list_grants_for_store("docs").await.unwrap();
    assert_eq!(for_store, vec![grant]);
}

#[tokio::test]
async fn grant_store_upserts_on_same_store_user_pair() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();
    // Re-grant with a different `granted_by` — must update, not duplicate.
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-2".to_string(),
            created_at: "2026-06-11T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    let grants = store.list_grants_for_user("u1").await.unwrap();
    assert_eq!(grants.len(), 1, "re-granting must not duplicate the row");
    assert_eq!(grants[0].granted_by, "admin-2");
}

#[tokio::test]
async fn revoke_store_grant_removes_it_and_reports_result() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    assert!(store.revoke_store_grant("docs", "u1").await.unwrap());
    assert!(store.list_grants_for_user("u1").await.unwrap().is_empty());
    assert!(!store.revoke_store_grant("docs", "u1").await.unwrap());
}

#[tokio::test]
async fn store_grant_cascades_on_user_delete() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store
        .create_user(&make_user("u1", "bob", Role::Member))
        .await
        .unwrap();
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "u1".to_string(),
            granted_by: "admin-1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    store.delete_user("u1").await.unwrap();

    assert!(
        store
            .list_grants_for_store("docs")
            .await
            .unwrap()
            .is_empty(),
        "grants must cascade-delete when their user is removed"
    );
}

// ---------------------------------------------------------------------
// Invites + access requests
// ---------------------------------------------------------------------

fn make_invite(id: &str, token_hash: &str, mode: InviteMode) -> InviteRow {
    InviteRow {
        id: id.to_string(),
        token_hash: token_hash.to_string(),
        mode,
        store_grants: vec!["docs".to_string()],
        max_uses: 1,
        uses: 0,
        expires_at: None,
        revoked_at: None,
        created_by: "admin-1".to_string(),
        created_at: "2026-06-10T12:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn create_and_find_invite_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    let invite = make_invite("i1", "inv-hash-1", InviteMode::Open);
    store.create_invite(&invite).await.unwrap();

    let found = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found, invite);

    let found_by_hash = store
        .find_invite_by_hash("inv-hash-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found_by_hash, invite);
}

#[tokio::test]
async fn list_invites_returns_all() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();
    store
        .create_invite(&make_invite("i2", "h2", InviteMode::Closed))
        .await
        .unwrap();
    assert_eq!(store.list_invites().await.unwrap().len(), 2);
}

#[tokio::test]
async fn revoke_invite_sets_revoked_at_once() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();
    assert!(store.revoke_invite("i1").await.unwrap());
    let found = store.find_invite("i1").await.unwrap().unwrap();
    assert!(found.revoked_at.is_some());
    assert!(!store.revoke_invite("i1").await.unwrap());
}

#[tokio::test]
async fn increment_invite_uses_increments() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();
    store.increment_invite_uses("i1").await.unwrap();
    store.increment_invite_uses("i1").await.unwrap();
    let found = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found.uses, 2);
}

#[tokio::test]
async fn create_and_find_access_request_round_trips() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Closed))
        .await
        .unwrap();

    let req = AccessRequestRow {
        id: "ar1".to_string(),
        invite_id: "i1".to_string(),
        requested_name: "carol".to_string(),
        secret_hash: "req-hash".to_string(),
        state: AccessRequestState::Pending,
        resulting_user_id: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        decided_at: None,
    };
    store.create_access_request(&req).await.unwrap();

    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(found, req);

    let for_invite = store.list_access_requests_for_invite("i1").await.unwrap();
    assert_eq!(for_invite, vec![req]);
}

#[tokio::test]
async fn update_access_request_state_approves_with_resulting_user() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Closed))
        .await
        .unwrap();
    store
        .create_user(&make_user("u1", "carol", Role::Member))
        .await
        .unwrap();
    store
        .create_access_request(&AccessRequestRow {
            id: "ar1".to_string(),
            invite_id: "i1".to_string(),
            requested_name: "carol".to_string(),
            secret_hash: "req-hash".to_string(),
            state: AccessRequestState::Pending,
            resulting_user_id: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            decided_at: None,
        })
        .await
        .unwrap();

    store
        .update_access_request_state(
            "ar1",
            AccessRequestState::Approved,
            Some("u1"),
            "2026-06-11T00:00:00Z",
        )
        .await
        .unwrap();

    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(found.state, AccessRequestState::Approved);
    assert_eq!(found.resulting_user_id.as_deref(), Some("u1"));
    assert_eq!(found.decided_at.as_deref(), Some("2026-06-11T00:00:00Z"));
}

#[tokio::test]
async fn access_requests_cascade_on_invite_delete() {
    // access_requests.invite_id has ON DELETE CASCADE; deleting the invite
    // (no `AuthStore` method for that yet — exercised directly against the
    // connection here) must remove its access requests too.
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Closed))
        .await
        .unwrap();
    store
        .create_access_request(&AccessRequestRow {
            id: "ar1".to_string(),
            invite_id: "i1".to_string(),
            requested_name: "carol".to_string(),
            secret_hash: "req-hash".to_string(),
            state: AccessRequestState::Pending,
            resulting_user_id: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            decided_at: None,
        })
        .await
        .unwrap();
    assert!(store.find_access_request("ar1").await.unwrap().is_some());

    {
        let conn = store.conn.conn().await;
        conn.execute("DELETE FROM invites WHERE id = 'i1'", ())
            .await
            .unwrap();
    }

    assert!(
        store.find_access_request("ar1").await.unwrap().is_none(),
        "access requests must cascade-delete when their invite is removed"
    );
}
