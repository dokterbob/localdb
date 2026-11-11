use super::*;
use localdb_core::auth::{AuthService, RedeemOutcome};
use std::sync::Arc;

#[tokio::test]
async fn bootstrap_is_atomic_resumable_and_never_reopens_established_admin() {
    let (_dir, _backend, store) = make_store().await;
    store.conn.writer().await.execute("CREATE TRIGGER fail_bootstrap BEFORE INSERT ON pending_bootstrap BEGIN SELECT RAISE(FAIL, 'injected'); END", ()).await.unwrap();
    let user = make_user("admin", "admin", Role::Admin);
    assert!(store.begin_bootstrap(&user).await.is_err());
    assert_eq!(store.count_users().await.unwrap(), 0);
    store
        .conn
        .writer()
        .await
        .execute("DROP TRIGGER fail_bootstrap", ())
        .await
        .unwrap();
    let other = make_user("other", "admin", Role::Admin);
    let (first, second) = tokio::join!(store.begin_bootstrap(&user), store.begin_bootstrap(&other));
    assert_eq!(first.unwrap().id, second.unwrap().id);
    assert_eq!(store.count_users().await.unwrap(), 1);
    assert_eq!(
        store.pending_bootstrap_user().await.unwrap().unwrap().id,
        user.id
    );
    store.complete_bootstrap(&user.id).await.unwrap();
    assert!(store.pending_bootstrap_user().await.unwrap().is_none());
    assert!(store.begin_bootstrap(&user).await.is_err());
}

#[tokio::test]
async fn begin_bootstrap_picks_an_unused_name_when_admin_is_taken() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_user(&make_user("m1", "admin", Role::Member))
        .await
        .unwrap();
    store
        .create_user(&make_user("m2", "admin-2", Role::Member))
        .await
        .unwrap();

    let bootstrapped = store
        .begin_bootstrap(&make_user("a1", "admin", Role::Admin))
        .await
        .unwrap();
    assert_eq!(bootstrapped.name, "admin-3");
    assert_eq!(store.get_user("a1").await.unwrap().unwrap().name, "admin-3");
}

#[tokio::test]
async fn rotation_rolls_back_failure_at_either_insert_and_can_retry() {
    for kind in ["refresh", "access"] {
        let (_dir, _backend, store) = make_store().await;
        let store = Arc::new(store);
        let svc = AuthService::new(store.clone());
        let user = svc.create_user("user", Role::Member).await.unwrap();
        let original = svc.issue_refresh_token(&user.id).await.unwrap();
        store.conn.writer().await.execute(&format!("CREATE TRIGGER fail_token BEFORE INSERT ON auth_tokens WHEN NEW.kind = '{kind}' BEGIN SELECT RAISE(FAIL, 'injected'); END"), ()).await.unwrap();
        assert!(svc.rotate_refresh_token(&original.secret).await.is_err());
        assert!(store
            .find_token(&original.row.id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_none());
        assert_eq!(store.list_tokens_for_user(&user.id).await.unwrap().len(), 1);
        store
            .conn
            .writer()
            .await
            .execute("DROP TRIGGER fail_token", ())
            .await
            .unwrap();
        let (access, refresh) = svc.rotate_refresh_token(&original.secret).await.unwrap();
        assert!(svc.authenticate(&access.secret).await.is_ok());
        assert!(svc.rotate_refresh_token(&refresh.secret).await.is_ok());
    }
}

#[tokio::test]
async fn approval_rolls_back_every_write_failure_and_can_retry() {
    for table in ["users", "store_grants", "access_requests"] {
        let (_dir, backend, store) = make_store().await;
        insert_store_row(&backend, "docs").await;
        let store = Arc::new(store);
        let svc = AuthService::new(store.clone());
        let invite = svc
            .create_invite(
                InviteMode::Closed,
                &[("docs".into(), StoreVisibility::Shared)],
                1,
                None,
                "admin",
            )
            .await
            .unwrap();
        let RedeemOutcome::Closed { request_id, .. } =
            svc.redeem_invite(&invite.secret, "reader").await.unwrap()
        else {
            panic!()
        };
        let mutation = if table == "access_requests" {
            "UPDATE"
        } else {
            "INSERT"
        };
        store.conn.writer().await.execute(&format!("CREATE TRIGGER fail_approval BEFORE {mutation} ON {table} BEGIN SELECT RAISE(FAIL, 'injected'); END"), ()).await.unwrap();
        assert!(svc.approve_request(&request_id).await.is_err());
        assert!(store.get_user_by_name("reader").await.unwrap().is_none());
        assert!(store
            .list_grants_for_store("docs")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .find_access_request(&request_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            AccessRequestState::Pending
        );
        store
            .conn
            .writer()
            .await
            .execute("DROP TRIGGER fail_approval", ())
            .await
            .unwrap();
        let user = svc.approve_request(&request_id).await.unwrap();
        assert_eq!(store.list_grants_for_user(&user.id).await.unwrap().len(), 1);
    }
}

fn make_invite(id: &str, token_hash: &str, mode: InviteMode) -> InviteRow {
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

/// `reserve_invite_use_on`'s own predicate (`revoked_at IS NULL AND
/// (expires_at IS NULL OR expires_at >= now)`) runs inside the transaction
/// that `redeem_open_invite`/`redeem_closed_invite` share. Going through
/// `AuthService::redeem_invite` never reaches it: the service rejects a
/// revoked or expired invite from its own initial read before ever calling
/// the store, so a test that revokes/expires the invite and then calls
/// `redeem_invite` only exercises the service's check, not the
/// transaction's. Calling the store methods directly is the only way to
/// exercise the transaction's own predicate, and also confirms a refused
/// redemption leaves no partial state (reserved use, minted user/request,
/// or inserted credential) behind.
#[tokio::test]
async fn redeem_open_invite_revoked_or_expired_leaves_no_partial_state() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    let user = make_user("u1", "alice", Role::Member);
    let credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");

    let mut revoked = make_invite("i1", "h1", InviteMode::Open);
    revoked.revoked_at = Some("2026-06-10T12:00:00Z".to_string());
    store.create_invite(&revoked).await.unwrap();
    assert!(!store
        .redeem_open_invite("i1", &user, &[], &credential)
        .await
        .unwrap());
    assert_eq!(store.find_invite("i1").await.unwrap().unwrap().uses, 0);
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_none());

    let mut expired = make_invite("i2", "h2", InviteMode::Open);
    expired.expires_at = Some("2000-01-01T00:00:00Z".to_string());
    store.create_invite(&expired).await.unwrap();
    assert!(!store
        .redeem_open_invite("i2", &user, &[], &credential)
        .await
        .unwrap());
    assert_eq!(store.find_invite("i2").await.unwrap().unwrap().uses, 0);
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn redeem_closed_invite_revoked_or_expired_leaves_no_partial_state() {
    let (_dir, _backend, store) = make_store().await;
    let request = AccessRequestRow {
        id: "ar1".to_string(),
        invite_id: "i1".to_string(),
        requested_name: "someone".to_string(),
        secret_hash: "req-hash".to_string(),
        state: AccessRequestState::Pending,
        resulting_user_id: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        decided_at: None,
        collected_at: None,
    };

    let mut revoked = make_invite("i1", "h1", InviteMode::Closed);
    revoked.revoked_at = Some("2026-06-10T12:00:00Z".to_string());
    store.create_invite(&revoked).await.unwrap();
    assert!(!store.redeem_closed_invite("i1", &request).await.unwrap());
    assert_eq!(store.find_invite("i1").await.unwrap().unwrap().uses, 0);
    assert!(store.find_access_request("ar1").await.unwrap().is_none());

    let mut expired = make_invite("i2", "h2", InviteMode::Closed);
    expired.expires_at = Some("2000-01-01T00:00:00Z".to_string());
    store.create_invite(&expired).await.unwrap();
    let request2 = AccessRequestRow {
        id: "ar2".to_string(),
        invite_id: "i2".to_string(),
        ..request
    };
    assert!(!store.redeem_closed_invite("i2", &request2).await.unwrap());
    assert_eq!(store.find_invite("i2").await.unwrap().unwrap().uses, 0);
    assert!(store.find_access_request("ar2").await.unwrap().is_none());
}

/// Finding #6/#7-class guard, direct-store coverage: a redemption whose
/// credential inherits a `minted_by_family` already recorded in
/// `auth_compromised_families` must be refused, not minted, even though the
/// invite use is otherwise eligible to reserve. Mirrors
/// `core::auth::store::tests::redeem_open_invite_refuses_a_credential_minted_by_a_compromised_family`
/// (the fake store's equivalent) — this is the real libsql transaction's own
/// `insert_token_on` check (`atomic.rs`'s `redeem_open_invite`), which that
/// fake-store test alone never exercises. Removing the `if
/// !insert_token_on(&tx, credential).await?` guard in
/// `redeem_open_invite` turns this test's `unwrap_err()` into a panic.
#[tokio::test]
async fn redeem_open_invite_refuses_a_credential_minted_by_a_compromised_family() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    store.revoke_compromised_family("fam-x").await.unwrap();
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();
    let user = make_user("u1", "alice", Role::Member);
    let mut credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");
    credential.minted_by_family = Some("fam-x".to_string());

    let err = store
        .redeem_open_invite("i1", &user, &[], &credential)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    assert_eq!(
        store.find_invite("i1").await.unwrap().unwrap().uses,
        0,
        "a refused redemption must not reserve a use"
    );
    assert!(store.get_user("u1").await.unwrap().is_none());
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_none());
}

/// Direct-store counterpart of `collect_access_request_refuses_a_credential_minted_by_a_compromised_family`
/// (the fake store's version, `core::auth::store::tests`) — exercises the
/// real libsql transaction's `insert_token_on` guard in
/// `atomic.rs`'s `collect_access_request`. Removing that guard turns this
/// test's `unwrap_err()` into a panic.
#[tokio::test]
async fn collect_access_request_refuses_a_credential_minted_by_a_compromised_family() {
    let (_dir, _backend, store) = make_store().await;
    store.revoke_compromised_family("fam-x").await.unwrap();
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
            collected_at: None,
        })
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
    let mut credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");
    credential.minted_by_family = Some("fam-x".to_string());

    let err = store
        .collect_access_request("ar1", "2026-06-11T00:00:01Z", &credential)
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
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn redeem_open_invite_rolls_back_every_write_failure_and_can_retry() {
    for table in ["users", "store_grants", "auth_tokens"] {
        let (_dir, backend, store) = make_store().await;
        insert_store_row(&backend, "docs").await;
        let store = Arc::new(store);
        let svc = AuthService::new(store.clone());
        let invite = svc
            .create_invite(
                InviteMode::Open,
                &[("docs".into(), StoreVisibility::Shared)],
                1,
                None,
                "admin",
            )
            .await
            .unwrap();
        store.conn.writer().await.execute(&format!("CREATE TRIGGER fail_redeem BEFORE INSERT ON {table} BEGIN SELECT RAISE(FAIL, 'injected'); END"), ()).await.unwrap();

        assert!(svc.redeem_invite(&invite.secret, "reader").await.is_err());
        assert!(store.get_user_by_name("reader").await.unwrap().is_none());
        assert!(store
            .list_grants_for_store("docs")
            .await
            .unwrap()
            .is_empty());
        let found = store.find_invite(&invite.row.id).await.unwrap().unwrap();
        assert_eq!(
            found.uses, 0,
            "a failed mint must roll back the reservation too, not just the insert it failed on"
        );

        store
            .conn
            .writer()
            .await
            .execute("DROP TRIGGER fail_redeem", ())
            .await
            .unwrap();
        let outcome = svc.redeem_invite(&invite.secret, "reader").await.unwrap();
        assert!(matches!(outcome, RedeemOutcome::Open { .. }));
    }
}

#[tokio::test]
async fn collect_access_request_rolls_back_the_credential_insert_on_failure_and_can_retry() {
    let (_dir, _backend, store) = make_store().await;
    let store = Arc::new(store);
    let svc = AuthService::new(store.clone());
    let invite = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin")
        .await
        .unwrap();
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = svc.redeem_invite(&invite.secret, "reader").await.unwrap()
    else {
        panic!("expected Closed outcome");
    };
    svc.approve_request(&request_id).await.unwrap();

    store.conn.writer().await.execute("CREATE TRIGGER fail_collect BEFORE INSERT ON auth_tokens BEGIN SELECT RAISE(FAIL, 'injected'); END", ()).await.unwrap();
    assert!(svc
        .poll_request(&request_id, &request_secret)
        .await
        .is_err());
    let found = store
        .find_access_request(&request_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        found.collected_at.is_none(),
        "a failed credential insert must roll back the collected_at write too"
    );

    store
        .conn
        .writer()
        .await
        .execute("DROP TRIGGER fail_collect", ())
        .await
        .unwrap();
    let outcome = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        localdb_core::auth::PollOutcome::Approved { .. }
    ));
}
