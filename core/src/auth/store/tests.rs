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

#[test]
fn first_unused_name_returns_base_when_free() {
    assert_eq!(first_unused_name("admin", |_| false), "admin");
}

#[test]
fn first_unused_name_skips_taken_suffixes() {
    let taken = ["admin", "admin-2", "admin-3"];
    assert_eq!(
        first_unused_name("admin", |n| taken.contains(&n)),
        "admin-4"
    );
}

#[tokio::test]
async fn begin_bootstrap_picks_an_unused_name_when_admin_is_taken() {
    let store = FakeAuthStore::new();
    store.create_user(&user("m1", "admin")).await.unwrap();

    let bootstrapped = store.begin_bootstrap(&admin("a1", "admin")).await.unwrap();
    assert_eq!(bootstrapped.name, "admin-2");
    assert_eq!(store.get_user("a1").await.unwrap().unwrap().name, "admin-2");
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
        minted_by_family: None,
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
            minted_by_family: None,
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

    assert_eq!(
        store.try_demote_user_unless_last_admin("a1").await.unwrap(),
        DemoteOutcome::LastAdmin
    );
    let reloaded = store.get_user("a1").await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Admin);
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_succeeds_with_another_admin() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "admin1")).await.unwrap();
    store.create_user(&admin("a2", "admin2")).await.unwrap();

    assert_eq!(
        store.try_demote_user_unless_last_admin("a1").await.unwrap(),
        DemoteOutcome::Done
    );
    let reloaded = store.get_user("a1").await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Member);
    // The remaining admin is unaffected.
    let a2 = store.get_user("a2").await.unwrap().unwrap();
    assert_eq!(a2.role, Role::Admin);
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_is_a_noop_done_on_a_member() {
    let store = FakeAuthStore::new();
    store.create_user(&admin("a1", "solo-admin")).await.unwrap();
    store.create_user(&user("m1", "some-member")).await.unwrap();

    // `m1` is already a member — the guarded write's "already a member"
    // branch matches, so this reports `Done` (a no-op success), not a
    // last-admin refusal and not a second, unguarded write.
    assert_eq!(
        store.try_demote_user_unless_last_admin("m1").await.unwrap(),
        DemoteOutcome::Done
    );
}

#[tokio::test]
async fn try_demote_user_unless_last_admin_reports_not_found_for_unknown_id() {
    let store = FakeAuthStore::new();
    assert_eq!(
        store
            .try_demote_user_unless_last_admin("nope")
            .await
            .unwrap(),
        DemoteOutcome::NotFound
    );
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

fn credential(id: &str, user_id: &str, secret_hash: &str) -> AuthTokenRow {
    AuthTokenRow {
        id: id.to_string(),
        user_id: user_id.to_string(),
        kind: super::TokenKind::ApiKey,
        secret_hash: secret_hash.to_string(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: "2026-06-11T00:00:00Z".to_string(),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    }
}

#[tokio::test]
async fn poison_next_collect_makes_collect_access_request_report_false_once() {
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
    let token = credential("t1", "u1", "hash-1");

    store.poison_next_collect("ar1").await;
    assert!(!store
        .collect_access_request("ar1", "2026-06-11T00:00:01Z", &token)
        .await
        .unwrap());
    let untouched = store.find_access_request("ar1").await.unwrap().unwrap();
    assert!(untouched.collected_at.is_none());
    assert!(
        store.find_token("t1").await.unwrap().is_none(),
        "a lost collect race must not mint anything"
    );

    // Single-shot: the next call behaves normally and mints the token.
    assert!(store
        .collect_access_request("ar1", "2026-06-11T00:00:02Z", &token)
        .await
        .unwrap());
    assert!(store.find_token("t1").await.unwrap().is_some());
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
        minted_by_family: None,
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

// -----------------------------------------------------------------
// redeem_open_invite/redeem_closed_invite: revoked/expired invites and
// credentials minted by a compromised family must be refused with no
// partial state left behind — mirrors store-libsql's equivalent tests.
// -----------------------------------------------------------------

fn invite(id: &str, token_hash: &str, mode: InviteMode) -> InviteRow {
    InviteRow {
        id: id.to_string(),
        token_hash: token_hash.to_string(),
        mode,
        store_grants: vec![],
        max_uses: 1,
        uses: 0,
        expires_at: None,
        revoked_at: None,
        created_by: "admin".to_string(),
        created_at: "2026-06-10T12:00:00Z".to_string(),
    }
}

#[tokio::test]
async fn redeem_open_invite_revoked_or_expired_leaves_no_partial_state() {
    let store = FakeAuthStore::new();
    let u = user("u1", "alice");
    let cred = credential("t1", "u1", "hash-cred");

    let mut revoked = invite("i1", "h1", InviteMode::Open);
    revoked.revoked_at = Some("2026-06-10T12:00:00Z".to_string());
    store.create_invite(&revoked).await.unwrap();
    assert!(!store
        .redeem_open_invite("i1", &u, &[], &cred)
        .await
        .unwrap());
    assert_eq!(store.find_invite("i1").await.unwrap().unwrap().uses, 0);
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(store.find_token("t1").await.unwrap().is_none());

    let mut expired = invite("i2", "h2", InviteMode::Open);
    expired.expires_at = Some("2000-01-01T00:00:00Z".to_string());
    store.create_invite(&expired).await.unwrap();
    assert!(!store
        .redeem_open_invite("i2", &u, &[], &cred)
        .await
        .unwrap());
    assert_eq!(store.find_invite("i2").await.unwrap().unwrap().uses, 0);
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(store.find_token("t1").await.unwrap().is_none());
}

#[tokio::test]
async fn redeem_closed_invite_revoked_or_expired_leaves_no_partial_state() {
    let store = FakeAuthStore::new();
    let request = access_request("ar1", "i1");

    let mut revoked = invite("i1", "h1", InviteMode::Closed);
    revoked.revoked_at = Some("2026-06-10T12:00:00Z".to_string());
    store.create_invite(&revoked).await.unwrap();
    assert!(!store.redeem_closed_invite("i1", &request).await.unwrap());
    assert_eq!(store.find_invite("i1").await.unwrap().unwrap().uses, 0);
    assert!(store.find_access_request("ar1").await.unwrap().is_none());

    let mut expired = invite("i2", "h2", InviteMode::Closed);
    expired.expires_at = Some("2000-01-01T00:00:00Z".to_string());
    store.create_invite(&expired).await.unwrap();
    let request2 = access_request("ar2", "i2");
    assert!(!store.redeem_closed_invite("i2", &request2).await.unwrap());
    assert_eq!(store.find_invite("i2").await.unwrap().unwrap().uses, 0);
    assert!(store.find_access_request("ar2").await.unwrap().is_none());
}

#[tokio::test]
async fn redeem_open_invite_refuses_a_credential_minted_by_a_compromised_family() {
    let store = FakeAuthStore::new();
    store.revoke_compromised_family("fam-x").await.unwrap();
    store
        .create_invite(&invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();
    let u = user("u1", "alice");
    let mut cred = credential("t1", "u1", "hash-cred");
    cred.minted_by_family = Some("fam-x".to_string());

    let err = store
        .redeem_open_invite("i1", &u, &[], &cred)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    assert_eq!(
        store.find_invite("i1").await.unwrap().unwrap().uses,
        0,
        "a refused redemption must not reserve a use"
    );
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(store.find_token("t1").await.unwrap().is_none());
}

#[tokio::test]
async fn collect_access_request_refuses_a_credential_minted_by_a_compromised_family() {
    let store = FakeAuthStore::new();
    store.revoke_compromised_family("fam-x").await.unwrap();
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
    let mut cred = credential("t1", "u1", "hash-cred");
    cred.minted_by_family = Some("fam-x".to_string());

    let err = store
        .collect_access_request("ar1", "2026-06-11T00:00:01Z", &cred)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    assert!(
        store
            .find_access_request("ar1")
            .await
            .unwrap()
            .unwrap()
            .collected_at
            .is_none(),
        "a refused collection must not mark the request collected"
    );
    assert!(store.find_token("t1").await.unwrap().is_none());
}
