use super::*;

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
async fn redeem_open_invite_commits_reservation_user_grants_and_credential_together() {
    let (_dir, backend, store) = make_store().await;
    insert_store_row(&backend, "docs").await;
    let mut invite = make_invite("i1", "h1", InviteMode::Open);
    invite.max_uses = 2;
    store.create_invite(&invite).await.unwrap();

    let user = make_user("u1", "alice", Role::Member);
    let grant = StoreGrantRow {
        store_name: "docs".to_string(),
        user_id: "u1".to_string(),
        granted_by: "admin-1".to_string(),
        created_at: "2026-06-10T12:00:00Z".to_string(),
    };
    let credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");

    assert!(store
        .redeem_open_invite("i1", &user, &[grant], &credential)
        .await
        .unwrap());

    assert!(store.get_user("u1").await.unwrap().is_some());
    assert_eq!(store.list_grants_for_user("u1").await.unwrap().len(), 1);
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_some());
    let found_invite = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found_invite.uses, 1, "exactly one use reserved, no more");
}

#[tokio::test]
async fn redeem_open_invite_refuses_once_exhausted_and_never_overshoots() {
    let (_dir, _backend, store) = make_store().await;
    let mut invite = make_invite("i1", "h1", InviteMode::Open);
    invite.max_uses = 1;
    store.create_invite(&invite).await.unwrap();
    let user = make_user("u1", "alice", Role::Member);
    let credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");
    assert!(store
        .redeem_open_invite("i1", &user, &[], &credential)
        .await
        .unwrap());

    // A tight loop of extra attempts never overshoots max_uses.
    for n in 0..5 {
        let extra_id = format!("extra-u{n}");
        let other = make_user(&extra_id, &format!("bob{n}"), Role::Member);
        let other_credential = make_token(
            &format!("extra-t{n}"),
            &extra_id,
            TokenKind::ApiKey,
            &format!("extra-hash{n}"),
        );
        assert!(!store
            .redeem_open_invite("i1", &other, &[], &other_credential)
            .await
            .unwrap());
        assert!(store.get_user(&extra_id).await.unwrap().is_none());
    }
    let found_invite = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(
        found_invite.uses, 1,
        "a refused redemption must not increment uses"
    );
}

#[tokio::test]
async fn redeem_open_invite_duplicate_name_rolls_back_the_whole_transaction() {
    let (_dir, _backend, store) = make_store().await;
    let mut invite = make_invite("i1", "h1", InviteMode::Open);
    invite.max_uses = 1;
    store.create_invite(&invite).await.unwrap();
    store
        .create_user(&make_user("existing", "taken", Role::Member))
        .await
        .unwrap();

    let user = make_user("u1", "taken", Role::Member);
    let credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");
    let err = store
        .redeem_open_invite("i1", &user, &[], &credential)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));

    let found_invite = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(
        found_invite.uses, 0,
        "a failed mint must roll back the reservation too, not just the user create"
    );
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_none());

    // Retrying with a fresh name against the same max_uses=1 invite succeeds.
    let retry_user = make_user("u2", "fresh-name", Role::Member);
    let retry_credential = make_token("t2", "u2", TokenKind::ApiKey, "hash-cred-2");
    assert!(store
        .redeem_open_invite("i1", &retry_user, &[], &retry_credential)
        .await
        .unwrap());
}

#[tokio::test]
async fn redeem_open_invite_revoked_or_expired_is_refused() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();
    store.revoke_invite("i1").await.unwrap();
    let user = make_user("u1", "alice", Role::Member);
    let credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");
    assert!(!store
        .redeem_open_invite("i1", &user, &[], &credential)
        .await
        .unwrap());

    let mut expired = make_invite("i2", "h2", InviteMode::Open);
    expired.expires_at = Some("2000-01-01T00:00:00Z".to_string());
    store.create_invite(&expired).await.unwrap();
    assert!(!store
        .redeem_open_invite("i2", &user, &[], &credential)
        .await
        .unwrap());
}

/// `reserve_invite_use_on`'s SQL boundary must match
/// `core::auth::is_expired`'s strict-earlier rule exactly: an invite
/// expiring in the same second as `now` is still eligible, one second
/// earlier is not. Calls `reserve_invite_use_on` directly with fixed
/// timestamps (mirroring `rotate_tokens_at`'s own pinned-boundary tests in
/// `tests/tokens.rs`) so the assertion never races the wall clock.
#[tokio::test]
async fn reserve_invite_use_on_boundary_matches_is_expired() {
    let (_dir, _backend, store) = make_store().await;
    let now = "2026-06-10T12:00:00Z";

    let mut at_now = make_invite("i1", "h1", InviteMode::Open);
    at_now.expires_at = Some(now.to_string());
    store.create_invite(&at_now).await.unwrap();

    let mut one_second_earlier = make_invite("i2", "h2", InviteMode::Open);
    one_second_earlier.expires_at = Some("2026-06-10T11:59:59Z".to_string());
    store.create_invite(&one_second_earlier).await.unwrap();

    let conn = store.conn.writer().await;
    assert!(
        crate::auth::invites::reserve_invite_use_on(&conn, "i1", now)
            .await
            .unwrap(),
        "an invite expiring in the exact same second as `now` must still be eligible"
    );
    assert!(
        !crate::auth::invites::reserve_invite_use_on(&conn, "i2", now)
            .await
            .unwrap(),
        "an invite that expired one second before `now` must be refused"
    );
}

#[tokio::test]
async fn redeem_closed_invite_commits_reservation_and_request_together() {
    let (_dir, _backend, store) = make_store().await;
    let mut invite = make_invite("i1", "h1", InviteMode::Closed);
    invite.max_uses = 1;
    store.create_invite(&invite).await.unwrap();

    let request = AccessRequestRow {
        id: "ar1".to_string(),
        invite_id: "i1".to_string(),
        requested_name: "carol".to_string(),
        secret_hash: "req-hash".to_string(),
        state: AccessRequestState::Pending,
        resulting_user_id: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        decided_at: None,
        collected_at: None,
    };
    assert!(store.redeem_closed_invite("i1", &request).await.unwrap());
    assert!(store.find_access_request("ar1").await.unwrap().is_some());
    let found_invite = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found_invite.uses, 1);

    // Now exhausted: a second request against the same invite is refused.
    let second = AccessRequestRow {
        id: "ar2".to_string(),
        ..request
    };
    assert!(!store.redeem_closed_invite("i1", &second).await.unwrap());
    assert!(store.find_access_request("ar2").await.unwrap().is_none());
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
        collected_at: None,
    };
    store.create_access_request(&req).await.unwrap();

    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(found, req);

    let for_invite = store.list_access_requests_for_invite("i1").await.unwrap();
    assert_eq!(for_invite, vec![req]);
}

#[tokio::test]
async fn try_decide_access_request_approves_with_resulting_user() {
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
            collected_at: None,
        })
        .await
        .unwrap();

    assert!(
        store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Approved,
                Some("u1"),
                "2026-06-11T00:00:00Z",
            )
            .await
            .unwrap(),
        "the first decision on a pending request must take effect"
    );

    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(found.state, AccessRequestState::Approved);
    assert_eq!(found.resulting_user_id.as_deref(), Some("u1"));
    assert_eq!(found.decided_at.as_deref(), Some("2026-06-11T00:00:00Z"));
}

/// Two decisions racing the same request must never both take effect — the
/// second (whichever it is) must observe the first's decision already
/// landed and report `false` rather than overwriting it.
#[tokio::test]
async fn try_decide_access_request_refuses_once_already_decided() {
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
            collected_at: None,
        })
        .await
        .unwrap();

    assert!(
        store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Denied,
                None,
                "2026-06-11T00:00:00Z",
            )
            .await
            .unwrap(),
        "the first (denying) decision must take effect"
    );

    // A second, differing decision (an approve racing the deny above) must
    // be refused, not overwrite the denial.
    assert!(
        !store
            .try_decide_access_request(
                "ar1",
                AccessRequestState::Approved,
                Some("u1"),
                "2026-06-11T00:00:01Z",
            )
            .await
            .unwrap(),
        "a second decision on an already-decided request must be refused"
    );

    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(
        found.state,
        AccessRequestState::Denied,
        "the original decision must be left untouched"
    );
    assert!(found.resulting_user_id.is_none());
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
            collected_at: None,
        })
        .await
        .unwrap();
    assert!(store.find_access_request("ar1").await.unwrap().is_some());

    {
        let conn = store.conn.writer().await;
        conn.execute("DELETE FROM invites WHERE id = 'i1'", ())
            .await
            .unwrap();
    }

    assert!(
        store.find_access_request("ar1").await.unwrap().is_none(),
        "access requests must cascade-delete when their invite is removed"
    );
}

#[tokio::test]
async fn list_access_requests_returns_every_request_across_invites() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Closed))
        .await
        .unwrap();
    store
        .create_invite(&make_invite("i2", "h2", InviteMode::Closed))
        .await
        .unwrap();
    store
        .create_access_request(&AccessRequestRow {
            id: "ar1".to_string(),
            invite_id: "i1".to_string(),
            requested_name: "carol".to_string(),
            secret_hash: "req-hash-1".to_string(),
            state: AccessRequestState::Pending,
            resulting_user_id: None,
            created_at: "2026-06-10T12:00:00Z".to_string(),
            decided_at: None,
            collected_at: None,
        })
        .await
        .unwrap();
    store
        .create_access_request(&AccessRequestRow {
            id: "ar2".to_string(),
            invite_id: "i2".to_string(),
            requested_name: "dave".to_string(),
            secret_hash: "req-hash-2".to_string(),
            state: AccessRequestState::Pending,
            resulting_user_id: None,
            created_at: "2026-06-10T12:00:01Z".to_string(),
            decided_at: None,
            collected_at: None,
        })
        .await
        .unwrap();

    let all = store.list_access_requests().await.unwrap();
    assert_eq!(all.len(), 2);
    let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"ar1"));
    assert!(ids.contains(&"ar2"));
}

#[tokio::test]
async fn collect_access_request_marks_collected_and_mints_the_credential_together() {
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
            collected_at: None,
        })
        .await
        .unwrap();
    let credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");

    // Not yet approved: collecting must fail and mint nothing.
    assert!(
        !store
            .collect_access_request("ar1", "2026-06-11T00:00:00Z", &credential)
            .await
            .unwrap(),
        "a pending request's credential cannot be collected"
    );
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_none());

    store
        .try_decide_access_request(
            "ar1",
            AccessRequestState::Approved,
            Some("u1"),
            "2026-06-11T00:00:00Z",
        )
        .await
        .unwrap();

    assert!(
        store
            .collect_access_request("ar1", "2026-06-11T00:00:01Z", &credential)
            .await
            .unwrap(),
        "the first collection attempt after approval must succeed"
    );
    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(found.collected_at.as_deref(), Some("2026-06-11T00:00:01Z"));
    assert!(store
        .find_token_by_hash("hash-cred")
        .await
        .unwrap()
        .is_some());

    // A second collection attempt must fail (single-use guard) and mint
    // nothing more.
    let second_credential = make_token("t2", "u1", TokenKind::ApiKey, "hash-cred-2");
    assert!(
        !store
            .collect_access_request("ar1", "2026-06-11T00:00:02Z", &second_credential)
            .await
            .unwrap(),
        "a second collection attempt must fail (single-use guard)"
    );
    assert!(store
        .find_token_by_hash("hash-cred-2")
        .await
        .unwrap()
        .is_none());
    // The timestamp from the first, successful collection is untouched.
    let found_again = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(
        found_again.collected_at.as_deref(),
        Some("2026-06-11T00:00:01Z")
    );
}

#[tokio::test]
async fn collect_access_request_unknown_id_returns_false() {
    let (_dir, _backend, store) = make_store().await;
    let credential = make_token("t1", "u1", TokenKind::ApiKey, "hash-cred");
    assert!(!store
        .collect_access_request("nonexistent", "2026-06-11T00:00:00Z", &credential)
        .await
        .unwrap());
}

/// `AuthService::redeem_invite`'s documented concurrency choice: a
/// double-redemption race can over-count an invite's `uses`, but it must
/// never double-mint a user under the same requested name — the
/// `users.name` UNIQUE constraint is the backstop that makes that safe, at
/// the store level, independent of whatever ordering the service layer
/// uses. This simulates two racing redeemers who both decided to use the
/// same requested name.
#[tokio::test]
async fn double_redeem_same_requested_name_collides_on_unique_constraint() {
    let (_dir, _backend, store) = make_store().await;
    let first = make_user("u1", "same-name", Role::Member);
    let second = make_user("u2", "same-name", Role::Member);

    store.create_user(&first).await.unwrap();
    let err = store.create_user(&second).await.unwrap_err();
    assert!(
        matches!(err, Error::InvalidRequest { .. }),
        "the second racer must get a well-formed InvalidRequest, not a double-mint: {err:?}"
    );

    // Exactly one user exists under that name.
    assert_eq!(
        store
            .list_users()
            .await
            .unwrap()
            .iter()
            .filter(|u| u.name == "same-name")
            .count(),
        1
    );
}
