use super::*;

#[tokio::test]
async fn create_invite_mints_a_show_once_secret() {
    let svc = service();
    let issued = svc
        .create_invite(
            InviteMode::Open,
            &[("docs".to_string(), StoreVisibility::Shared)],
            1,
            None,
            "admin-1",
        )
        .await
        .unwrap();
    assert!(issued.secret.starts_with(TOKEN_PREFIX));
    assert_eq!(issued.row.uses, 0);
    assert_eq!(issued.row.max_uses, 1);
    assert_eq!(issued.row.store_grants, vec!["docs".to_string()]);
}

#[tokio::test]
async fn create_invite_rejects_private_store_grant() {
    let svc = service();
    let err = svc
        .create_invite(
            InviteMode::Open,
            &[("secret-store".to_string(), StoreVisibility::Private)],
            1,
            None,
            "admin-1",
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Forbidden { .. }));
}

#[tokio::test]
async fn create_invite_rejects_zero_max_uses() {
    let svc = service();
    let err = svc
        .create_invite(InviteMode::Open, &[], 0, None, "admin-1")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn redeem_open_invite_happy_path_creates_user_grants_and_credential() {
    let svc = service();
    let issued = make_open_invite(&svc, 1).await;

    let outcome = svc.redeem_invite(&issued.secret, "newbie").await.unwrap();
    let RedeemOutcome::Open {
        user,
        grants,
        credential,
    } = outcome
    else {
        panic!("expected Open outcome");
    };
    assert_eq!(user.name, "newbie");
    assert_eq!(user.role, Role::Member);
    assert_eq!(grants, vec!["docs".to_string()]);
    assert!(credential.secret.starts_with(TOKEN_PREFIX));

    // The credential actually authenticates as the new user with the grant.
    let principal = svc.authenticate(&credential.secret).await.unwrap();
    assert_eq!(principal.user_id, user.id);
    assert!(principal.can_read_store("docs", StoreVisibility::Shared));

    let invite = svc
        .store
        .find_invite(&issued.row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invite.uses, 1);
}

#[tokio::test]
async fn redeem_open_invite_max_uses_one_double_redeem_fails() {
    let svc = service();
    let issued = make_open_invite(&svc, 1).await;

    svc.redeem_invite(&issued.secret, "first").await.unwrap();
    let err = svc
        .redeem_invite(&issued.secret, "second")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn redeem_open_invite_max_uses_two_allows_two_distinct_names() {
    let svc = service();
    let issued = make_open_invite(&svc, 2).await;

    svc.redeem_invite(&issued.secret, "first").await.unwrap();
    svc.redeem_invite(&issued.secret, "second").await.unwrap();
    let invite = svc
        .store
        .find_invite(&issued.row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invite.uses, 2);
}

#[tokio::test]
async fn redeem_invite_rejects_duplicate_requested_name() {
    let svc = service();
    svc.create_user("taken", Role::Member).await.unwrap();
    let issued = make_open_invite(&svc, 5).await;

    let err = svc
        .redeem_invite(&issued.secret, "taken")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn redeem_open_invite_failed_mint_does_not_burn_use_and_retry_succeeds() {
    // Pins findings #3 + #7: a failed redemption (duplicate name
    // colliding with `create_user`'s UNIQUE constraint) must not
    // permanently consume the invite's only use — a fresh name must
    // still be able to redeem the same `max_uses = 1` invite.
    let svc = service();
    svc.create_user("taken", Role::Member).await.unwrap();
    let issued = make_open_invite(&svc, 1).await;

    let err = svc
        .redeem_invite(&issued.secret, "taken")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));

    let invite = svc
        .store
        .find_invite(&issued.row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        invite.uses, 0,
        "a failed redemption must not burn the reserved use"
    );

    // Retrying with a fresh name against the same max_uses=1 invite
    // must still succeed.
    let outcome = svc
        .redeem_invite(&issued.secret, "fresh-name")
        .await
        .unwrap();
    assert!(matches!(outcome, RedeemOutcome::Open { .. }));

    let invite = svc
        .store
        .find_invite(&issued.row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invite.uses, 1);

    // And the invite is now genuinely exhausted.
    let err = svc
        .redeem_invite(&issued.secret, "yet-another")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn redeem_open_invite_mint_failure_after_create_user_cleans_up_orphan() {
    // Pins finding #6: if a post-create step (`issue_api_key` here,
    // simulated via the `poison_next_insert_token` hook) fails after
    // `create_user` already succeeded, the user must not be left
    // behind — otherwise a retry with the same name would fail forever
    // with "user already exists", and an uncredentialed member would
    // linger with no way to reach it.
    let svc = service();
    let issued = make_open_invite(&svc, 1).await;

    svc.store.poison_next_insert_token().await;

    let err = svc
        .redeem_invite(&issued.secret, "newbie")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Internal { .. }));

    // No orphaned user left behind.
    assert!(svc
        .store
        .get_user_by_name("newbie")
        .await
        .unwrap()
        .is_none());

    // The invite use was released (composes with the existing
    // reserve/release behavior), not permanently burned.
    let invite = svc
        .store
        .find_invite(&issued.row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        invite.uses, 0,
        "a failed mint must not burn the reserved invite use"
    );

    // Retrying with the SAME name now succeeds — proof the orphaned
    // user was actually removed, not just that the invite use reset.
    let outcome = svc.redeem_invite(&issued.secret, "newbie").await.unwrap();
    let RedeemOutcome::Open {
        user, credential, ..
    } = outcome
    else {
        panic!("expected Open outcome");
    };
    assert_eq!(user.name, "newbie");
    // The retry's credential actually authenticates.
    svc.authenticate(&credential.secret).await.unwrap();
}

#[tokio::test]
async fn redeem_closed_invite_consumes_a_use_on_success() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;

    svc.redeem_invite(&issued.secret, "requester")
        .await
        .unwrap();

    let invite = svc
        .store
        .find_invite(&issued.row.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(invite.uses, 1);

    // The invite is now exhausted for a second requester.
    let err = svc
        .redeem_invite(&issued.secret, "another")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn redeem_invite_unknown_token_fails() {
    let svc = service();
    let err = svc
        .redeem_invite("ldb_not-a-real-invite", "someone")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn redeem_invite_revoked_fails() {
    let svc = service();
    let issued = make_open_invite(&svc, 1).await;
    svc.store.revoke_invite(&issued.row.id).await.unwrap();

    let err = svc
        .redeem_invite(&issued.secret, "someone")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn redeem_invite_expired_fails() {
    let svc = service();
    let minted = mint_secret();
    let invite = InviteRow {
        id: new_ulid(),
        token_hash: minted.hash,
        mode: InviteMode::Open,
        store_grants: vec![],
        max_uses: 1,
        uses: 0,
        expires_at: Some(rfc3339_from_now(-10)),
        revoked_at: None,
        created_by: "admin-1".to_string(),
        created_at: now_rfc3339(),
    };
    svc.store.create_invite(&invite).await.unwrap();

    let err = svc
        .redeem_invite(&minted.secret, "someone")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn redeem_closed_invite_happy_path_files_pending_request() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;

    let outcome = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap();
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = outcome
    else {
        panic!("expected Closed outcome");
    };
    assert!(request_secret.starts_with(TOKEN_PREFIX));

    let poll = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(poll, PollOutcome::Pending);
}

#[tokio::test]
async fn closed_invite_approve_then_poll_once_then_already_collected() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    let user = svc.approve_request(&request_id).await.unwrap();
    assert_eq!(user.name, "requester");
    assert_eq!(user.role, Role::Member);
    assert!(svc.store.get_user(&user.id).await.unwrap().is_some());

    // First poll after approval: a freshly minted credential is handed back.
    let first = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    let PollOutcome::Approved { credential } = first else {
        panic!("expected Approved on first post-approval poll, got {first:?}");
    };
    assert_ne!(
        credential, request_secret,
        "the poll-only request secret must never become the live credential"
    );

    // The freshly minted credential actually authenticates as the newly
    // created user, with the invite's store grants applied.
    let principal = svc.authenticate(&credential).await.unwrap();
    assert_eq!(principal.user_id, user.id);
    assert!(principal.can_read_store("docs", StoreVisibility::Shared));

    // Second poll: terminal "already collected" state, not a credential again.
    let second = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(second, PollOutcome::AlreadyCollected);
}

#[tokio::test]
async fn poll_request_mint_failure_does_not_permanently_brick_collection() {
    // Pins finding #7: if `issue_api_key` fails on the collecting poll
    // (simulated via `poison_next_insert_token`), `collected_at` must
    // NOT have been set — otherwise every subsequent poll would
    // observe `AlreadyCollected` forever and the requester could never
    // get a credential.
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };
    svc.approve_request(&request_id).await.unwrap();

    svc.store.poison_next_insert_token().await;
    let err = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Internal { .. }));

    // Not bricked: a retry after the transient failure still succeeds
    // and hands back a real credential (not `AlreadyCollected`).
    let retry = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    let PollOutcome::Approved { credential } = retry else {
        panic!("expected Approved on retry after mint failure, got {retry:?}");
    };
    svc.authenticate(&credential).await.unwrap();

    // Exactly-once still holds: a third poll is terminal.
    let third = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(third, PollOutcome::AlreadyCollected);
}

#[tokio::test]
async fn poll_request_lost_collect_race_revokes_its_own_freshly_minted_key() {
    // Pins finding #7's other half: a poll that mints successfully but
    // then loses the atomic `mark_access_request_collected` race (a
    // concurrent poll collected first — simulated via
    // `poison_next_collect`) must not leave its own freshly minted key
    // live: exactly one *usable* credential must exist afterward.
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };
    let user = svc.approve_request(&request_id).await.unwrap();

    svc.store.poison_next_collect(&request_id).await;
    let outcome = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(outcome, PollOutcome::AlreadyCollected);

    // The key minted during the losing attempt must be revoked, not
    // left live and unclaimed.
    let tokens = svc.store.list_tokens_for_user(&user.id).await.unwrap();
    assert_eq!(tokens.len(), 1);
    assert!(
        tokens[0].revoked_at.is_some(),
        "a lost collect race must revoke its own freshly minted key"
    );
}

#[tokio::test]
async fn closed_invite_request_secret_never_authenticates() {
    // Pin: the request secret is poll-only, both before and after
    // collection — it must never itself work as a bearer credential,
    // since it travels as a URL query parameter on every poll (access
    // logs, proxies, shell history).
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    // Not a credential while pending.
    let err = svc.authenticate(&request_secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    svc.approve_request(&request_id).await.unwrap();

    // Not a credential immediately after approval either (live from
    // approval time was exactly the defect being fixed).
    let err = svc.authenticate(&request_secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    svc.poll_request(&request_id, &request_secret)
        .await
        .unwrap();

    // Still not a credential after the collecting poll.
    let err = svc.authenticate(&request_secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn closed_invite_approve_without_poll_mints_no_api_key_token() {
    // Pin: the durable credential is born only at first successful
    // collection, never merely by approval — an approved-but-never
    // -polled request must leave no API-key token row for its user.
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    let user = svc.approve_request(&request_id).await.unwrap();

    let tokens = svc.store.list_tokens_for_user(&user.id).await.unwrap();
    assert!(
        tokens.is_empty(),
        "approval alone must not mint any credential for the new user"
    );
}

#[tokio::test]
async fn closed_invite_deny_then_poll_denied() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    svc.deny_request(&request_id).await.unwrap();
    let poll = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(poll, PollOutcome::Denied);

    // No user was created.
    assert!(svc
        .store
        .get_user_by_name("requester")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn approve_request_twice_fails() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    svc.approve_request(&request_id).await.unwrap();
    let err = svc.approve_request(&request_id).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn approve_request_lost_atomic_race_deletes_the_orphaned_user_and_errors() {
    // Simulates a concurrent decision winning the atomic
    // `try_decide_access_request` race after `approve_request` has
    // already created the user and applied grants: the pre-check
    // passed (the request looked pending), but the store-level
    // conditional UPDATE reports "no-op" (poisoned here to stand in for
    // a real concurrent winner). The user created for the losing
    // attempt must not survive.
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    svc.store.poison_next_decide(&request_id).await;

    let err = svc.approve_request(&request_id).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));

    // No orphaned user left behind, despite `create_user` +
    // `apply_invite_grants` having already succeeded before the lost
    // race was discovered.
    assert!(svc
        .store
        .get_user_by_name("requester")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn deny_request_lost_atomic_race_errors_without_side_effects() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    svc.store.poison_next_decide(&request_id).await;

    let err = svc.deny_request(&request_id).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));

    // `deny_request` never creates a user regardless; pin that too.
    assert!(svc
        .store
        .get_user_by_name("requester")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn approve_request_normal_path_still_works_after_atomicity_fix() {
    // Regression guard: the atomic guard must not disturb the ordinary
    // single-caller happy path.
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    let user = svc.approve_request(&request_id).await.unwrap();
    assert_eq!(user.name, "requester");
    let request = svc
        .store
        .find_access_request(&request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.state, AccessRequestState::Approved);
    assert_eq!(request.resulting_user_id.as_deref(), Some(user.id.as_str()));
}

#[tokio::test]
async fn deny_request_normal_path_still_works_after_atomicity_fix() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    svc.deny_request(&request_id).await.unwrap();
    let request = svc
        .store
        .find_access_request(&request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.state, AccessRequestState::Denied);
}

#[tokio::test]
async fn deny_request_unknown_id_fails() {
    let svc = service();
    let err = svc.deny_request("nonexistent").await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

#[tokio::test]
async fn poll_request_wrong_secret_fails() {
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    let err = svc
        .poll_request(&request_id, "ldb_wrong-secret")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn poll_request_unknown_id_and_wrong_secret_are_indistinguishable() {
    // No existence oracle (specs/05-surfaces.md §3.1): both an unknown
    // request id and a wrong secret against a real one must produce the
    // exact same error shape.
    let svc = service();
    let issued = make_closed_invite(&svc, 1).await;
    let RedeemOutcome::Closed { request_id, .. } = svc
        .redeem_invite(&issued.secret, "requester")
        .await
        .unwrap()
    else {
        panic!("expected Closed outcome");
    };

    let unknown_id_err = svc
        .poll_request("totally-unknown-id", "ldb_whatever")
        .await
        .unwrap_err();
    let wrong_secret_err = svc
        .poll_request(&request_id, "ldb_wrong-secret")
        .await
        .unwrap_err();

    assert!(matches!(unknown_id_err, Error::Unauthorized { .. }));
    assert!(matches!(wrong_secret_err, Error::Unauthorized { .. }));
    assert_eq!(unknown_id_err.to_string(), wrong_secret_err.to_string());
}

#[tokio::test]
async fn create_invite_with_grants_on_shared_store_then_redeem_grants_access() {
    let svc = service();
    let issued = svc
        .create_invite(
            InviteMode::Open,
            &[("docs".to_string(), StoreVisibility::Shared)],
            1,
            None,
            "admin-1",
        )
        .await
        .unwrap();
    let RedeemOutcome::Open { user, .. } =
        svc.redeem_invite(&issued.secret, "grantee").await.unwrap()
    else {
        panic!("expected Open outcome");
    };
    let grants = svc.store.list_grants_for_user(&user.id).await.unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].store_name, "docs");
}

#[tokio::test]
async fn invite_expiry_is_validated_and_normalized() {
    let svc = service();
    for value in ["tomorrow", "2026-99-99T00:00:00Z", ""] {
        assert!(matches!(
            svc.create_invite(InviteMode::Open, &[], 1, Some(value.into()), "admin")
                .await,
            Err(Error::InvalidRequest { .. })
        ));
    }
    let invite = svc
        .create_invite(
            InviteMode::Open,
            &[],
            1,
            Some("2030-01-01T01:00:00+01:00".into()),
            "admin",
        )
        .await
        .unwrap();
    assert_eq!(
        invite.row.expires_at.as_deref(),
        Some("2030-01-01T00:00:00Z")
    );
}
