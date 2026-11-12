//! `closed`-mode invite request/approve/poll cases (device-authorization
//! -grant pattern, RFC 8628) shared by every `AuthStore` implementation.

use std::sync::Arc;

use crate::auth::{
    AccessRequestState, AuthService, AuthStore, InviteMode, PollOutcome, RedeemOutcome, Role,
};
use crate::types::StoreVisibility;
use crate::Error;

pub async fn redeem_files_pending_request_and_consumes_a_use<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-pending")
        .await
        .unwrap();
    let RedeemOutcome::Closed { request_id, .. } = outcome else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };

    let request = store
        .find_access_request(&request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request.state, AccessRequestState::Pending);
    assert_eq!(request.requested_name, "invites-closed-pending");
    let reloaded_invite = store.find_invite(&issued.row.id).await.unwrap().unwrap();
    assert_eq!(reloaded_invite.uses, 1);
}

pub async fn approve_creates_user_and_grants<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(
            InviteMode::Closed,
            &[(
                super::SHARED_STORE_NAME.to_string(),
                StoreVisibility::Shared,
            )],
            1,
            None,
            "admin-1",
        )
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-approve")
        .await
        .unwrap();
    let RedeemOutcome::Closed { request_id, .. } = outcome else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };

    let user = svc.approve_request(&request_id).await.unwrap();
    assert_eq!(user.name, "invites-closed-approve");
    assert!(store
        .list_grants_for_user(&user.id)
        .await
        .unwrap()
        .iter()
        .any(|g| g.store_name == super::SHARED_STORE_NAME));
}

pub async fn approve_with_taken_name_fails_and_leaves_request_pending<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    svc.create_user("invites-closed-taken", Role::Member)
        .await
        .unwrap();
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-taken")
        .await
        .unwrap();
    let RedeemOutcome::Closed { request_id, .. } = outcome else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };

    let err = svc.approve_request(&request_id).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
    let reloaded = store
        .find_access_request(&request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded.state,
        AccessRequestState::Pending,
        "a failed approval must leave the request pending, not half-decided"
    );
}

pub async fn deny_sets_denied_outcome<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-deny")
        .await
        .unwrap();
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = outcome
    else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };

    svc.deny_request(&request_id).await.unwrap();
    let poll = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(poll, PollOutcome::Denied);
}

pub async fn poll_before_decision_is_pending<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-poll-pending")
        .await
        .unwrap();
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = outcome
    else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };

    let poll = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(poll, PollOutcome::Pending);
}

pub async fn first_poll_after_approval_returns_key_once_then_already_collected<S: AuthStore>(
    store: Arc<S>,
) {
    let svc = AuthService::new(store);
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-collect")
        .await
        .unwrap();
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = outcome
    else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };
    let user = svc.approve_request(&request_id).await.unwrap();

    let first = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    let PollOutcome::Approved { credential } = first else {
        panic!("the first poll after approval must observe PollOutcome::Approved");
    };
    let principal = svc.authenticate(&credential).await.unwrap();
    assert_eq!(principal.user_id, user.id);

    let second = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert_eq!(second, PollOutcome::AlreadyCollected);
}

pub async fn wrong_secret_and_unknown_id_are_indistinguishable<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-indistinguishable")
        .await
        .unwrap();
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = outcome
    else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };
    // Approved, not merely pending: a wrong-secret poll against a request
    // that is actually ready to hand out a credential must still be refused
    // identically to an unknown id — and must not disturb the credential
    // the right secret is still owed.
    svc.approve_request(&request_id).await.unwrap();

    let wrong_secret_err = svc
        .poll_request(&request_id, "not-the-real-secret")
        .await
        .unwrap_err();
    let unknown_id_err = svc
        .poll_request("totally-unknown-request-id", "whatever-secret")
        .await
        .unwrap_err();
    assert!(matches!(wrong_secret_err, Error::Unauthorized { .. }));
    assert!(matches!(unknown_id_err, Error::Unauthorized { .. }));
    assert_eq!(
        wrong_secret_err.to_string(),
        unknown_id_err.to_string(),
        "a wrong secret and an unknown request id must be indistinguishable"
    );

    // The approved request is still fully collectable by its real secret.
    let poll = svc
        .poll_request(&request_id, &request_secret)
        .await
        .unwrap();
    assert!(matches!(poll, PollOutcome::Approved { .. }));
}

/// Deterministic concurrency check: a burst of simultaneous polls against
/// one approved, not-yet-collected request must yield exactly one
/// `Approved` credential and every other poller `AlreadyCollected`.
pub async fn concurrent_polls_after_approval_yield_exactly_one_key<S: AuthStore>(store: Arc<S>) {
    const POLLERS: usize = 10;
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(InviteMode::Closed, &[], 1, None, "admin-1")
        .await
        .unwrap();
    let outcome = svc
        .redeem_invite(&issued.secret, "invites-closed-concurrent-poll")
        .await
        .unwrap();
    let RedeemOutcome::Closed {
        request_id,
        request_secret,
    } = outcome
    else {
        panic!("closed-mode redemption must return RedeemOutcome::Closed");
    };
    let user = svc.approve_request(&request_id).await.unwrap();

    let barrier = Arc::new(tokio::sync::Barrier::new(POLLERS));
    let mut handles = Vec::new();
    for _ in 0..POLLERS {
        let store = store.clone();
        let request_id = request_id.clone();
        let request_secret = request_secret.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            AuthService::new(store)
                .poll_request(&request_id, &request_secret)
                .await
        }));
    }
    let mut approved = 0u32;
    let mut already_collected = 0u32;
    let mut winning_credential = None;
    for h in handles {
        match h.await.unwrap().unwrap() {
            PollOutcome::Approved { credential } => {
                approved += 1;
                winning_credential = Some(credential);
            }
            PollOutcome::AlreadyCollected => already_collected += 1,
            other => panic!("unexpected poll outcome under a concurrent burst: {other:?}"),
        }
    }
    assert_eq!(
        approved, 1,
        "exactly one concurrent poller must collect the credential"
    );
    assert_eq!(already_collected, POLLERS as u32 - 1);

    let svc = AuthService::new(store.clone());
    let winning_credential = winning_credential.expect("the one winner must have a credential");
    let principal = svc.authenticate(&winning_credential).await.unwrap();
    assert_eq!(principal.user_id, user.id);
    assert_eq!(
        store.list_tokens_for_user(&user.id).await.unwrap().len(),
        1,
        "exactly one API key must be persisted for the new user"
    );
}
