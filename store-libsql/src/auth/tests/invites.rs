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
async fn try_consume_invite_use_increments_up_to_max_uses_then_fails() {
    let (_dir, _backend, store) = make_store().await;
    let mut invite = make_invite("i1", "h1", InviteMode::Open);
    invite.max_uses = 2;
    store.create_invite(&invite).await.unwrap();

    assert!(store.try_consume_invite_use("i1").await.unwrap());
    assert!(store.try_consume_invite_use("i1").await.unwrap());
    // Third call: already at max_uses == uses, must not increment further.
    assert!(!store.try_consume_invite_use("i1").await.unwrap());
    // A tight loop of extra attempts never overshoots max_uses either.
    for _ in 0..5 {
        assert!(!store.try_consume_invite_use("i1").await.unwrap());
    }

    let found = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found.uses, 2, "uses must never exceed max_uses");
}

#[tokio::test]
async fn release_invite_use_decrements_a_reserved_use() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();

    assert!(store.try_consume_invite_use("i1").await.unwrap());
    let found = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found.uses, 1);

    store.release_invite_use("i1").await.unwrap();
    let found = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found.uses, 0, "release must give the reserved slot back");

    // The released slot can be reserved again.
    assert!(store.try_consume_invite_use("i1").await.unwrap());
}

#[tokio::test]
async fn release_invite_use_never_goes_negative() {
    let (_dir, _backend, store) = make_store().await;
    store
        .create_invite(&make_invite("i1", "h1", InviteMode::Open))
        .await
        .unwrap();

    // Releasing with nothing reserved must not drive uses below zero.
    store.release_invite_use("i1").await.unwrap();
    let found = store.find_invite("i1").await.unwrap().unwrap();
    assert_eq!(found.uses, 0);
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

/// Finding #4: two decisions racing the same request must never both take
/// effect — the second (whichever it is) must observe the first's decision
/// already landed and report `false` rather than overwriting it.
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
async fn mark_access_request_collected_succeeds_once_then_fails() {
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

    // Not yet approved: collecting must fail.
    assert!(
        !store
            .mark_access_request_collected("ar1", "2026-06-11T00:00:00Z")
            .await
            .unwrap(),
        "a pending request's credential cannot be collected"
    );

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
            .mark_access_request_collected("ar1", "2026-06-11T00:00:01Z")
            .await
            .unwrap(),
        "the first collection attempt after approval must succeed"
    );
    let found = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(found.collected_at.as_deref(), Some("2026-06-11T00:00:01Z"));

    assert!(
        !store
            .mark_access_request_collected("ar1", "2026-06-11T00:00:02Z")
            .await
            .unwrap(),
        "a second collection attempt must fail (single-use guard)"
    );
    // The timestamp from the first, successful collection is untouched.
    let found_again = store.find_access_request("ar1").await.unwrap().unwrap();
    assert_eq!(
        found_again.collected_at.as_deref(),
        Some("2026-06-11T00:00:01Z")
    );
}

#[tokio::test]
async fn mark_access_request_collected_unknown_id_returns_false() {
    let (_dir, _backend, store) = make_store().await;
    assert!(!store
        .mark_access_request_collected("nonexistent", "2026-06-11T00:00:00Z")
        .await
        .unwrap());
}

/// T6's documented concurrency choice (`AuthService::redeem_invite`'s doc
/// comment): a double-redemption race can over-count an invite's `uses`,
/// but it must never double-mint a user under the same requested name — the
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
