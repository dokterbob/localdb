//! Direct `AuthStore` cases for the `closed`-mode invite/access-request
//! methods `AuthService` never exercises directly, or only incidentally:
//! `redeem_closed_invite`'s own eligibility guard, `create_access_request`,
//! `list_access_requests_for_invite`, `list_access_requests`'s documented
//! ordering, `try_decide_access_request`'s unknown/already-decided cases,
//! `approve_access_request` against a non-pending request, and
//! `collect_access_request` against a pending/denied/unknown request.
//! Also the concurrency cases for closed-mode redemption and for a
//! request's approve/deny race.

use std::sync::Arc;

use crate::auth::{
    mint_secret, rfc3339_from_now, AccessRequestRow, AccessRequestState, AuthService, AuthStore,
    AuthTokenRow, InviteMode, InviteRow, RedeemOutcome, Role, StoreGrantRow, TokenKind, UserRow,
};
use crate::ids::new_ulid;
use crate::Error;

fn seed_invite(
    mode: InviteMode,
    max_uses: u32,
    uses: u32,
    mutate: impl FnOnce(&mut InviteRow),
) -> InviteRow {
    let mut row = InviteRow {
        id: new_ulid(),
        token_hash: mint_secret().hash,
        mode,
        store_grants: Vec::new(),
        max_uses,
        uses,
        expires_at: None,
        revoked_at: None,
        created_by: "admin-1".to_string(),
        created_at: rfc3339_from_now(-10),
    };
    mutate(&mut row);
    row
}

fn seed_request(invite_id: &str, name: &str, created_at: &str) -> AccessRequestRow {
    AccessRequestRow {
        id: new_ulid(),
        invite_id: invite_id.to_string(),
        requested_name: name.to_string(),
        secret_hash: mint_secret().hash,
        state: AccessRequestState::Pending,
        resulting_user_id: None,
        created_at: created_at.to_string(),
        decided_at: None,
        collected_at: None,
    }
}

/// A freshly minted API-key row for `user_id`, the shape
/// `AuthService::poll_request` would build for an access request's
/// resulting user.
fn credential_for(user_id: &str) -> AuthTokenRow {
    AuthTokenRow {
        id: new_ulid(),
        user_id: user_id.to_string(),
        kind: TokenKind::ApiKey,
        secret_hash: mint_secret().hash,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    }
}

/// A pending request taken all the way to `Approved` via the direct store
/// call (not through the service), with a real resulting user — the
/// fixture every `collect_access_request` direct case needs.
async fn seed_approved_request<S: AuthStore>(
    store: &S,
    name_prefix: &str,
) -> (AccessRequestRow, UserRow) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        &format!("{name_prefix}-request"),
        &rfc3339_from_now(0),
    );
    store.create_access_request(&request).await.unwrap();
    let user = UserRow {
        id: new_ulid(),
        name: format!("{name_prefix}-user"),
        role: Role::Member,
        created_at: rfc3339_from_now(0),
    };
    let approved = store
        .approve_access_request(&request.id, &user, &[])
        .await
        .unwrap();
    assert!(
        approved,
        "test setup: approving the fresh request must succeed"
    );
    (request, user)
}

pub async fn redeem_closed_invite_direct_on_revoked_invite_is_refused<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |i| {
        i.revoked_at = Some(rfc3339_from_now(0));
    });
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-revoked",
        &rfc3339_from_now(0),
    );

    let ok = store
        .redeem_closed_invite(&invite.id, &request)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store
        .find_access_request(&request.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.find_invite(&invite.id).await.unwrap().unwrap().uses,
        0
    );
}

pub async fn redeem_closed_invite_direct_on_expired_invite_is_refused<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |i| {
        i.expires_at = Some(rfc3339_from_now(-3600));
    });
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-expired",
        &rfc3339_from_now(0),
    );

    let ok = store
        .redeem_closed_invite(&invite.id, &request)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store
        .find_access_request(&request.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.find_invite(&invite.id).await.unwrap().unwrap().uses,
        0
    );
}

pub async fn redeem_closed_invite_direct_on_exhausted_invite_is_refused<S: AuthStore>(
    store: Arc<S>,
) {
    let invite = seed_invite(InviteMode::Closed, 2, 2, |_| {});
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-exhausted",
        &rfc3339_from_now(0),
    );

    let ok = store
        .redeem_closed_invite(&invite.id, &request)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store
        .find_access_request(&request.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.find_invite(&invite.id).await.unwrap().unwrap().uses,
        2
    );
}

pub async fn create_access_request_direct_persists_it<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 1, 0, |_| {});
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-create",
        &rfc3339_from_now(0),
    );

    store.create_access_request(&request).await.unwrap();
    let reloaded = store
        .find_access_request(&request.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded, request);
}

pub async fn list_access_requests_for_invite_filters_by_invite<S: AuthStore>(store: Arc<S>) {
    let invite_a = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    let invite_b = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    store.create_invite(&invite_a).await.unwrap();
    store.create_invite(&invite_b).await.unwrap();
    let req_a = seed_request(
        &invite_a.id,
        "invites-closed-store-list-a",
        &rfc3339_from_now(0),
    );
    let req_b = seed_request(
        &invite_b.id,
        "invites-closed-store-list-b",
        &rfc3339_from_now(0),
    );
    store.create_access_request(&req_a).await.unwrap();
    store.create_access_request(&req_b).await.unwrap();

    let listed = store
        .list_access_requests_for_invite(&invite_a.id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, req_a.id);
}

/// `list_access_requests`' doc comment promises newest-created last.
pub async fn list_access_requests_orders_newest_last<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    store.create_invite(&invite).await.unwrap();
    let oldest = seed_request(
        &invite.id,
        "invites-closed-store-order-1",
        "2026-01-01T00:00:00Z",
    );
    let middle = seed_request(
        &invite.id,
        "invites-closed-store-order-2",
        "2026-01-01T00:00:01Z",
    );
    let newest = seed_request(
        &invite.id,
        "invites-closed-store-order-3",
        "2026-01-01T00:00:02Z",
    );
    // Inserted out of chronological order, on purpose: the ordering must
    // come from `created_at`, not insertion order.
    store.create_access_request(&newest).await.unwrap();
    store.create_access_request(&oldest).await.unwrap();
    store.create_access_request(&middle).await.unwrap();

    let all = store.list_access_requests().await.unwrap();
    let ids: Vec<&str> = all
        .iter()
        .filter(|r| r.invite_id == invite.id)
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![oldest.id.as_str(), middle.id.as_str(), newest.id.as_str()]
    );
}

pub async fn try_decide_access_request_unknown_id_is_false<S: AuthStore>(store: Arc<S>) {
    let decided = store
        .try_decide_access_request(
            "invites-closed-store-unknown-decide",
            AccessRequestState::Approved,
            None,
            &rfc3339_from_now(0),
        )
        .await
        .unwrap();
    assert!(!decided);
}

pub async fn try_decide_access_request_already_decided_is_false<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-redecide",
        &rfc3339_from_now(0),
    );
    store.create_access_request(&request).await.unwrap();
    let first_decided_at = rfc3339_from_now(0);
    assert!(store
        .try_decide_access_request(
            &request.id,
            AccessRequestState::Denied,
            None,
            &first_decided_at
        )
        .await
        .unwrap());

    let second = store
        .try_decide_access_request(
            &request.id,
            AccessRequestState::Approved,
            None,
            &rfc3339_from_now(0),
        )
        .await
        .unwrap();
    assert!(
        !second,
        "a request already decided must refuse a second decision"
    );
    let reloaded = store
        .find_access_request(&request.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.state, AccessRequestState::Denied);
    assert_eq!(reloaded.decided_at, Some(first_decided_at));
}

pub async fn approve_access_request_direct_on_non_pending_request_is_false<S: AuthStore>(
    store: Arc<S>,
) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-approve-nonpending",
        &rfc3339_from_now(0),
    );
    store.create_access_request(&request).await.unwrap();
    assert!(store
        .try_decide_access_request(
            &request.id,
            AccessRequestState::Denied,
            None,
            &rfc3339_from_now(0)
        )
        .await
        .unwrap());

    let user = UserRow {
        id: new_ulid(),
        name: "invites-closed-store-approve-nonpending-user".to_string(),
        role: Role::Member,
        created_at: rfc3339_from_now(0),
    };
    let ok = store
        .approve_access_request(&request.id, &user, &[])
        .await
        .unwrap();
    assert!(!ok);
    assert!(store.get_user(&user.id).await.unwrap().is_none());
}

pub async fn collect_access_request_on_pending_request_is_false<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-collect-pending",
        &rfc3339_from_now(0),
    );
    store.create_access_request(&request).await.unwrap();

    let credential = crate::auth::AuthTokenRow {
        id: new_ulid(),
        user_id: new_ulid(),
        kind: crate::auth::TokenKind::ApiKey,
        secret_hash: mint_secret().hash,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    };
    let ok = store
        .collect_access_request(&request.id, &rfc3339_from_now(0), &credential)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store.find_token(&credential.id).await.unwrap().is_none());
}

pub async fn collect_access_request_on_denied_request_is_false<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |_| {});
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-collect-denied",
        &rfc3339_from_now(0),
    );
    store.create_access_request(&request).await.unwrap();
    assert!(store
        .try_decide_access_request(
            &request.id,
            AccessRequestState::Denied,
            None,
            &rfc3339_from_now(0)
        )
        .await
        .unwrap());

    let credential = crate::auth::AuthTokenRow {
        id: new_ulid(),
        user_id: new_ulid(),
        kind: crate::auth::TokenKind::ApiKey,
        secret_hash: mint_secret().hash,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    };
    let ok = store
        .collect_access_request(&request.id, &rfc3339_from_now(0), &credential)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store.find_token(&credential.id).await.unwrap().is_none());
}

pub async fn collect_access_request_unknown_id_is_false<S: AuthStore>(store: Arc<S>) {
    let credential = crate::auth::AuthTokenRow {
        id: new_ulid(),
        user_id: new_ulid(),
        kind: crate::auth::TokenKind::ApiKey,
        secret_hash: mint_secret().hash,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    };
    let ok = store
        .collect_access_request(
            "invites-closed-store-collect-unknown",
            &rfc3339_from_now(0),
            &credential,
        )
        .await
        .unwrap();
    assert!(!ok);
    assert!(store.find_token(&credential.id).await.unwrap().is_none());
}

/// `collect_access_request` called twice in sequence against the same
/// approved request: the first call succeeds, the second observes the
/// documented not-collectable outcome, and only the first credential is
/// ever persisted.
pub async fn collect_access_request_direct_twice_sequential_succeeds_then_refuses<S: AuthStore>(
    store: Arc<S>,
) {
    let (request, user) =
        seed_approved_request(&*store, "invites-closed-store-collect-twice").await;

    let first_credential = credential_for(&user.id);
    let first = store
        .collect_access_request(&request.id, &rfc3339_from_now(0), &first_credential)
        .await
        .unwrap();
    assert!(
        first,
        "the first collect of a freshly approved request must succeed"
    );

    let second_credential = credential_for(&user.id);
    let second = store
        .collect_access_request(&request.id, &rfc3339_from_now(0), &second_credential)
        .await
        .unwrap();
    assert!(
        !second,
        "a second collect of an already-collected request must refuse"
    );

    assert!(store
        .find_token(&first_credential.id)
        .await
        .unwrap()
        .is_some());
    assert!(store
        .find_token(&second_credential.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.list_tokens_for_user(&user.id).await.unwrap().len(),
        1,
        "exactly one credential must ever be persisted for the user"
    );
}

/// Deterministic concurrency check that calls the STORE directly, not the
/// service: `AuthService::poll_request` reads the request's `collected_at`
/// itself before ever calling `collect_access_request`, so a loser can
/// return `PollOutcome::AlreadyCollected` from that read alone, without the
/// store's own atomic guard ever running — a store with no consume-once
/// guard at all could still pass a service-level burst if every loser
/// happened to observe the winner's write first. Driving
/// `collect_access_request` directly, with every contender's own prepared
/// credential row, forces every task to actually reach the store.
pub async fn collect_access_request_direct_concurrent_collect_yields_one_credential<
    S: AuthStore,
>(
    store: Arc<S>,
) {
    const CONTENDERS: usize = 8;
    let (request, user) =
        seed_approved_request(&*store, "invites-closed-store-collect-concurrent").await;

    let barrier = Arc::new(tokio::sync::Barrier::new(CONTENDERS));
    let mut handles = Vec::new();
    let mut credential_ids = Vec::new();
    for _ in 0..CONTENDERS {
        let store = store.clone();
        let request_id = request.id.clone();
        let barrier = barrier.clone();
        let credential = credential_for(&user.id);
        credential_ids.push(credential.id.clone());
        handles.push(tokio::spawn(async move {
            let collected_at = rfc3339_from_now(0);
            barrier.wait().await;
            store
                .collect_access_request(&request_id, &collected_at, &credential)
                .await
        }));
    }
    let mut successes = 0u32;
    for h in handles {
        match h.await.unwrap() {
            Ok(true) => successes += 1,
            Ok(false) => {}
            Err(e) => panic!("unexpected error racing collect_access_request: {e:?}"),
        }
    }
    assert_eq!(successes, 1, "exactly one concurrent collect must succeed");

    let reloaded = store
        .find_access_request(&request.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        reloaded.collected_at.is_some(),
        "the request must end up collected regardless of which contender won"
    );

    let mut existing = 0u32;
    for id in &credential_ids {
        if store.find_token(id).await.unwrap().is_some() {
            existing += 1;
        }
    }
    assert_eq!(
        existing, 1,
        "exactly one of the prepared credential rows must exist — the winner's"
    );
    assert_eq!(
        store.list_tokens_for_user(&user.id).await.unwrap().len(),
        1,
        "exactly one credential row must exist for the user"
    );
}

/// Deterministic concurrency check: a burst of simultaneous closed-mode
/// redemptions against one `max_uses` invite must file exactly `max_uses`
/// pending requests — never more, regardless of scheduling order.
pub async fn concurrent_closed_redemption_enforces_max_uses<S: AuthStore>(store: Arc<S>) {
    const MAX_USES: u32 = 3;
    const BURST: usize = 10;
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(InviteMode::Closed, &[], MAX_USES, None, "admin-1")
        .await
        .unwrap();

    let barrier = Arc::new(tokio::sync::Barrier::new(BURST));
    let mut handles = Vec::new();
    for i in 0..BURST {
        let store = store.clone();
        let secret = issued.secret.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            AuthService::new(store)
                .redeem_invite(&secret, &format!("invites-closed-store-burst-{i}"))
                .await
        }));
    }
    let mut filed = 0u32;
    let mut refused = 0u32;
    for h in handles {
        match h.await.unwrap() {
            Ok(RedeemOutcome::Closed { .. }) => filed += 1,
            Err(Error::Unauthorized { .. }) => refused += 1,
            other => panic!("unexpected redeem_invite outcome under a concurrent burst: {other:?}"),
        }
    }
    assert_eq!(filed, MAX_USES, "exactly max_uses requests must be filed");
    assert_eq!(refused, BURST as u32 - MAX_USES);
    let reloaded = store.find_invite(&issued.row.id).await.unwrap().unwrap();
    assert_eq!(reloaded.uses, MAX_USES);
    let requests = store
        .list_access_requests_for_invite(&issued.row.id)
        .await
        .unwrap();
    assert_eq!(requests.len() as u32, MAX_USES);
}

/// Deterministic concurrency check: an approve and a deny racing the same
/// pending request must leave exactly one decision standing.
pub async fn concurrent_approve_and_deny_yields_exactly_one_decision<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-store-approve-deny-race")
        .await
        .unwrap();
    let RedeemOutcome::Closed { request_id, .. } = outcome else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };
    let pending = store
        .find_access_request(&request_id)
        .await
        .unwrap()
        .unwrap();
    let requested_name = pending.requested_name.clone();

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let approve_store = store.clone();
    let approve_request_id = request_id.clone();
    let approve_barrier = barrier.clone();
    let approve = tokio::spawn(async move {
        approve_barrier.wait().await;
        AuthService::new(approve_store)
            .approve_request(&approve_request_id)
            .await
    });
    let deny_store = store.clone();
    let deny_request_id = request_id.clone();
    let deny_barrier = barrier.clone();
    let deny = tokio::spawn(async move {
        deny_barrier.wait().await;
        AuthService::new(deny_store)
            .deny_request(&deny_request_id)
            .await
    });

    let approve_outcome = approve.await.unwrap();
    let deny_outcome = deny.await.unwrap();
    let approve_won = match &approve_outcome {
        Ok(_) => true,
        Err(Error::InvalidRequest { .. }) => false,
        Err(other) => panic!("unexpected error racing approve_request: {other:?}"),
    };
    let deny_won = match &deny_outcome {
        Ok(()) => true,
        Err(Error::InvalidRequest { .. }) => false,
        Err(other) => panic!("unexpected error racing deny_request: {other:?}"),
    };
    assert_eq!(
        [approve_won, deny_won].into_iter().filter(|w| *w).count(),
        1,
        "exactly one of approve/deny must win the race"
    );

    let reloaded = store
        .find_access_request(&request_id)
        .await
        .unwrap()
        .unwrap();
    if approve_won {
        assert_eq!(reloaded.state, AccessRequestState::Approved);
        assert!(reloaded.resulting_user_id.is_some());
        assert!(
            store
                .get_user_by_name(&requested_name)
                .await
                .unwrap()
                .is_some(),
            "the winning approve must have created the user"
        );
    } else {
        assert_eq!(reloaded.state, AccessRequestState::Denied);
        assert!(
            reloaded.resulting_user_id.is_none(),
            "a denied request must have no resulting user"
        );
        assert!(
            store
                .get_user_by_name(&requested_name)
                .await
                .unwrap()
                .is_none(),
            "a losing approve must leave no orphan user behind"
        );
    }
}

/// Deterministic concurrency check that calls the STORE directly: one task
/// approves (with its own prepared user + grants) while another denies the
/// same pending request. `AuthService::approve_request`/`deny_request` both
/// pre-check the request via `find_pending_request` before ever calling the
/// store, so a loser can return from that read alone — driving
/// `approve_access_request`/`try_decide_access_request` directly forces
/// both contenders to actually reach the store's own atomic guard.
pub async fn approve_vs_deny_direct_concurrent_yields_one_decision<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Closed, 5, 0, |i| {
        i.store_grants = vec![super::SHARED_STORE_NAME.to_string()];
    });
    store.create_invite(&invite).await.unwrap();
    let request = seed_request(
        &invite.id,
        "invites-closed-store-approve-deny-direct",
        &rfc3339_from_now(0),
    );
    store.create_access_request(&request).await.unwrap();

    let user = UserRow {
        id: new_ulid(),
        name: "invites-closed-store-approve-deny-direct-user".to_string(),
        role: Role::Member,
        created_at: rfc3339_from_now(0),
    };
    let grants = vec![StoreGrantRow {
        store_name: super::SHARED_STORE_NAME.to_string(),
        user_id: user.id.clone(),
        granted_by: invite.created_by.clone(),
        created_at: rfc3339_from_now(0),
    }];

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let approve_store = store.clone();
    let approve_request_id = request.id.clone();
    let approve_user = user.clone();
    let approve_grants = grants.clone();
    let approve_barrier = barrier.clone();
    let approve = tokio::spawn(async move {
        approve_barrier.wait().await;
        approve_store
            .approve_access_request(&approve_request_id, &approve_user, &approve_grants)
            .await
    });
    let deny_store = store.clone();
    let deny_request_id = request.id.clone();
    let deny_barrier = barrier.clone();
    let deny = tokio::spawn(async move {
        deny_barrier.wait().await;
        deny_store
            .try_decide_access_request(
                &deny_request_id,
                AccessRequestState::Denied,
                None,
                &rfc3339_from_now(0),
            )
            .await
    });

    let approve_won = match approve.await.unwrap() {
        Ok(ok) => ok,
        Err(e) => panic!("unexpected error racing approve_access_request: {e:?}"),
    };
    let deny_won = match deny.await.unwrap() {
        Ok(ok) => ok,
        Err(e) => panic!("unexpected error racing try_decide_access_request: {e:?}"),
    };
    assert_eq!(
        [approve_won, deny_won].into_iter().filter(|w| *w).count(),
        1,
        "exactly one of approve/deny must win the race"
    );

    let reloaded = store
        .find_access_request(&request.id)
        .await
        .unwrap()
        .unwrap();
    if approve_won {
        assert_eq!(reloaded.state, AccessRequestState::Approved);
        assert!(store.get_user(&user.id).await.unwrap().is_some());
        assert!(
            !store
                .list_grants_for_user(&user.id)
                .await
                .unwrap()
                .is_empty(),
            "the winning approve's grants must have been applied"
        );
    } else {
        assert_eq!(reloaded.state, AccessRequestState::Denied);
        assert!(
            store.get_user(&user.id).await.unwrap().is_none(),
            "a losing approve must not create its user"
        );
        assert!(
            store
                .list_grants_for_user(&user.id)
                .await
                .unwrap()
                .is_empty(),
            "a losing approve must not apply its grants"
        );
    }
}
