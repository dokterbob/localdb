use super::*;

fn user(id: &str, name: &str) -> UserRow {
    UserRow {
        id: id.to_string(),
        name: name.to_string(),
        role: Role::Member,
        created_at: "2026-06-10T12:00:00Z".to_string(),
    }
}

fn admin(id: &str, name: &str) -> UserRow {
    UserRow {
        role: Role::Admin,
        ..user(id, name)
    }
}

#[tokio::test]
async fn create_user_rejects_duplicate_name() {
    let store = FakeAuthStore::new();
    store.create_user(&user("u1", "alice")).await.unwrap();
    let err = store.create_user(&user("u2", "alice")).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn poison_next_revoke_makes_revoke_token_report_false_once() {
    let store = FakeAuthStore::new();
    let token = AuthTokenRow {
        id: "t1".to_string(),
        user_id: "u1".to_string(),
        kind: super::TokenKind::Refresh,
        secret_hash: "hash-1".to_string(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        family_id: None,
        rotated_from: None,
    };
    store.insert_token(&token).await.unwrap();

    store.poison_next_revoke("t1").await;
    assert!(!store.revoke_token("t1").await.unwrap());
    // `revoked_at` itself must remain untouched by the poisoned call.
    let stored = store.find_token("t1").await.unwrap().unwrap();
    assert!(stored.revoked_at.is_none());

    // The poison is single-shot: the next call behaves normally.
    assert!(store.revoke_token("t1").await.unwrap());
}

// -----------------------------------------------------------------
// T5/finding #5: atomic last-admin guard at the store level
// -----------------------------------------------------------------

#[tokio::test]
async fn try_delete_user_unless_last_admin_refuses_the_sole_admin() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "only-admin")).await.unwrap();

    assert!(!store.try_delete_user_unless_last_admin("a1").await.unwrap());
    assert!(store.get_user("a1").await.unwrap().is_some());
}

#[tokio::test]
async fn try_delete_user_unless_last_admin_succeeds_with_another_admin() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "admin1")).await.unwrap();
    store.create_user(&admin("a2", "admin2")).await.unwrap();

    assert!(store.try_delete_user_unless_last_admin("a1").await.unwrap());
    assert!(store.get_user("a1").await.unwrap().is_none());
    assert!(store.get_user("a2").await.unwrap().is_some());
}

#[tokio::test]
async fn try_delete_user_unless_last_admin_allows_deleting_a_member() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "solo-admin")).await.unwrap();
    store.create_user(&user("m1", "some-member")).await.unwrap();

    // The guard only ever blocks dropping the *admin* count to zero —
    // deleting a member is unaffected even with only one admin present.
    assert!(store.try_delete_user_unless_last_admin("m1").await.unwrap());
    assert!(store.get_user("m1").await.unwrap().is_none());
}

#[tokio::test]
async fn try_delete_user_unless_last_admin_cascades_tokens_and_grants() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "admin1")).await.unwrap();
    store.create_user(&admin("a2", "admin2")).await.unwrap();
    store
        .insert_token(&AuthTokenRow {
            id: "t1".to_string(),
            user_id: "a1".to_string(),
            kind: super::TokenKind::ApiKey,
            secret_hash: "hash-1".to_string(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            family_id: None,
            rotated_from: None,
        })
        .await
        .unwrap();
    store
        .grant_store(&StoreGrantRow {
            store_name: "docs".to_string(),
            user_id: "a1".to_string(),
            granted_by: "a2".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();

    assert!(store.try_delete_user_unless_last_admin("a1").await.unwrap());
    assert!(store.list_tokens_for_user("a1").await.unwrap().is_empty());
    assert!(store.list_grants_for_user("a1").await.unwrap().is_empty());
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_refuses_the_sole_admin() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "only-admin")).await.unwrap();

    assert!(!store.try_demote_user_unless_last_admin("a1").await.unwrap());
    let reloaded = store.get_user("a1").await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Admin);
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_succeeds_with_another_admin() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "admin1")).await.unwrap();
    store.create_user(&admin("a2", "admin2")).await.unwrap();

    assert!(store.try_demote_user_unless_last_admin("a1").await.unwrap());
    let reloaded = store.get_user("a1").await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Member);
    // The remaining admin is unaffected.
    let a2 = store.get_user("a2").await.unwrap().unwrap();
    assert_eq!(a2.role, Role::Admin);
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_is_a_noop_on_a_member() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "solo-admin")).await.unwrap();
    store.create_user(&user("m1", "some-member")).await.unwrap();

    // `m1` isn't an admin at all — the guard's WHERE clause never
    // matches, so this reports `false` (nothing to demote), not a
    // last-admin refusal.
    assert!(!store.try_demote_user_unless_last_admin("m1").await.unwrap());
}

#[tokio::test]
async fn count_users_reflects_inserts() {
    let store = FakeAuthStore::new();
    assert_eq!(store.count_users().await.unwrap(), 0);
    store.create_user(&user("u1", "alice")).await.unwrap();
    assert_eq!(store.count_users().await.unwrap(), 1);
}

// -----------------------------------------------------------------
// Finding #5: `admin_exists` must key off role, not mere presence.
// -----------------------------------------------------------------

#[tokio::test]
async fn admin_exists_is_false_with_zero_users() {
    let store = FakeAuthStore::new();
    assert!(!store.admin_exists().await.unwrap());
}

#[tokio::test]
async fn admin_exists_is_false_with_only_member_users() {
    let store = FakeAuthStore::new();
    store.create_user(&user("m1", "some-member")).await.unwrap();
    assert!(!store.admin_exists().await.unwrap());
}

#[tokio::test]
async fn admin_exists_is_true_once_an_admin_is_present() {
    let store = FakeAuthStore::new();
    store.create_user(&user("m1", "some-member")).await.unwrap();
    store.create_user(&admin("a1", "an-admin")).await.unwrap();
    assert!(store.admin_exists().await.unwrap());
}

fn access_request(id: &str, invite_id: &str) -> AccessRequestRow {
    AccessRequestRow {
        id: id.to_string(),
        invite_id: invite_id.to_string(),
        requested_name: "someone".to_string(),
        secret_hash: "req-hash".to_string(),
        state: AccessRequestState::Pending,
        resulting_user_id: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        decided_at: None,
        collected_at: None,
    }
}

#[tokio::test]
async fn try_decide_access_request_transitions_pending_once() {
    let store = FakeAuthStore::new();
    store
        .create_access_request(&access_request("ar1", "i1"))
        .await
        .unwrap();

    assert!(store
        .try_decide_access_request(
            "ar1",
            AccessRequestState::Approved,
            Some("u1"),
            "2026-06-11T00:00:00Z",
        )
        .await
        .unwrap());
    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(found.state, AccessRequestState::Approved);
    assert_eq!(found.resulting_user_id.as_deref(), Some("u1"));

    // Already decided: a second decision must not overwrite it.
    assert!(!store
        .try_decide_access_request(
            "ar1",
            AccessRequestState::Denied,
            None,
            "2026-06-11T00:00:01Z",
        )
        .await
        .unwrap());
    let unchanged = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(unchanged.state, AccessRequestState::Approved);
}

#[tokio::test]
async fn poison_next_decide_makes_try_decide_report_false_once() {
    let store = FakeAuthStore::new();
    store
        .create_access_request(&access_request("ar1", "i1"))
        .await
        .unwrap();

    store.poison_next_decide("ar1").await;
    assert!(!store
        .try_decide_access_request(
            "ar1",
            AccessRequestState::Approved,
            Some("u1"),
            "2026-06-11T00:00:00Z",
        )
        .await
        .unwrap());
    let untouched = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(untouched.state, AccessRequestState::Pending);

    // Single-shot: the next call behaves normally.
    assert!(store
        .try_decide_access_request(
            "ar1",
            AccessRequestState::Approved,
            Some("u1"),
            "2026-06-11T00:00:01Z",
        )
        .await
        .unwrap());
}

#[tokio::test]
async fn poison_next_collect_makes_mark_collected_report_false_once() {
    let store = FakeAuthStore::new();
    store
        .create_access_request(&access_request("ar1", "i1"))
        .await
        .unwrap();
    store
        .try_decide_access_request(
            "ar1",
            AccessRequestState::Approved,
            Some("u1"),
            "2026-06-11T00:00:00Z",
        )
        .await
        .unwrap();

    store.poison_next_collect("ar1").await;
    assert!(!store
        .mark_access_request_collected("ar1", "2026-06-11T00:00:01Z")
        .await
        .unwrap());
    let untouched = store.find_access_request("ar1").await.unwrap().unwrap();
    assert!(untouched.collected_at.is_none());

    // Single-shot: the next call behaves normally.
    assert!(store
        .mark_access_request_collected("ar1", "2026-06-11T00:00:02Z")
        .await
        .unwrap());
}

#[tokio::test]
async fn poison_next_insert_token_fails_the_next_insert_only() {
    let store = FakeAuthStore::new();
    let token = AuthTokenRow {
        id: "t1".to_string(),
        user_id: "u1".to_string(),
        kind: super::TokenKind::ApiKey,
        secret_hash: "hash-1".to_string(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        family_id: None,
        rotated_from: None,
    };

    store.poison_next_insert_token().await;
    assert!(store.insert_token(&token).await.is_err());
    assert!(store.find_token("t1").await.unwrap().is_none());

    // Single-shot: the next call behaves normally.
    store.insert_token(&token).await.unwrap();
    assert!(store.find_token("t1").await.unwrap().is_some());
}

#[tokio::test]
async fn revoke_store_grant_returns_false_when_absent() {
    let store = FakeAuthStore::new();
    assert!(!store.revoke_store_grant("docs", "u1").await.unwrap());
}

#[tokio::test]
async fn grant_store_is_idempotent_per_store_user_pair() {
    let store = FakeAuthStore::new();
    let grant = StoreGrantRow {
        store_name: "docs".to_string(),
        user_id: "u1".to_string(),
        granted_by: "admin".to_string(),
        created_at: "2026-06-10T12:00:00Z".to_string(),
    };
    store.grant_store(&grant).await.unwrap();
    store.grant_store(&grant).await.unwrap();
    let grants = store.list_grants_for_user("u1").await.unwrap();
    assert_eq!(grants.len(), 1, "re-granting must not duplicate the row");
}
