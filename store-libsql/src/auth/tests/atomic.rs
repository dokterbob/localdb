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

#[tokio::test]
async fn invite_reservation_rechecks_revocation_and_expiry() {
    let (_dir, _backend, store) = make_store().await;
    let store = Arc::new(store);
    let svc = AuthService::new(store.clone());
    let invite = svc
        .create_invite(InviteMode::Open, &[], 1, None, "admin")
        .await
        .unwrap();
    // Simulates revocation between the service's initial read and
    // reservation: `redeem_invite` reads the invite, finds it not revoked,
    // then calls the store, by which point it has been revoked.
    store.revoke_invite(&invite.row.id).await.unwrap();
    let err = svc
        .redeem_invite(&invite.secret, "someone")
        .await
        .unwrap_err();
    assert!(matches!(err, localdb_core::Error::Unauthorized { .. }));

    let expired = svc
        .create_invite(
            InviteMode::Open,
            &[],
            1,
            Some("2000-01-01T00:00:00Z".into()),
            "admin",
        )
        .await
        .unwrap();
    let err = svc
        .redeem_invite(&expired.secret, "someone")
        .await
        .unwrap_err();
    assert!(matches!(err, localdb_core::Error::Unauthorized { .. }));
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
