//! `open`-mode invite redemption cases shared by every `AuthStore`
//! implementation.

use std::sync::Arc;

use crate::auth::rfc3339_from_now;
use crate::auth::{AuthService, AuthStore, InviteMode, RedeemOutcome};
use crate::types::StoreVisibility;
use crate::Error;

pub async fn redeem_creates_user_grants_and_key_atomically<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(
            InviteMode::Open,
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
        .redeem_invite(&issued.secret, "invites-open-happy")
        .await
        .unwrap();
    let RedeemOutcome::Open {
        user,
        grants,
        credential,
    } = outcome
    else {
        panic!("open-mode redemption must return RedeemOutcome::Open");
    };
    assert_eq!(user.name, "invites-open-happy");
    assert_eq!(grants, vec![super::SHARED_STORE_NAME.to_string()]);

    let principal = svc.authenticate(&credential.secret).await.unwrap();
    assert_eq!(principal.user_id, user.id);
    assert!(store
        .list_grants_for_user(&user.id)
        .await
        .unwrap()
        .iter()
        .any(|g| g.store_name == super::SHARED_STORE_NAME));
}

pub async fn max_uses_enforced_sequentially<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(InviteMode::Open, &[], 2, None, "admin-1")
        .await
        .unwrap();

    svc.redeem_invite(&issued.secret, "invites-open-seq-1")
        .await
        .unwrap();
    svc.redeem_invite(&issued.secret, "invites-open-seq-2")
        .await
        .unwrap();
    let err = svc
        .redeem_invite(&issued.secret, "invites-open-seq-3")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    let reloaded = store.find_invite(&issued.row.id).await.unwrap().unwrap();
    assert_eq!(reloaded.uses, 2);
}

/// Deterministic concurrency check: a burst of simultaneous redemptions
/// against one `max_uses` invite must yield exactly `max_uses` successes —
/// never more, regardless of scheduling order.
pub async fn max_uses_enforced_under_concurrent_burst<S: AuthStore>(store: Arc<S>) {
    const MAX_USES: u32 = 3;
    const BURST: usize = 10;
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(InviteMode::Open, &[], MAX_USES, None, "admin-1")
        .await
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..BURST {
        let store = store.clone();
        let secret = issued.secret.clone();
        handles.push(tokio::spawn(async move {
            AuthService::new(store)
                .redeem_invite(&secret, &format!("invites-open-burst-{i}"))
                .await
        }));
    }
    let mut successes = 0u32;
    for h in handles {
        if h.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(
        successes, MAX_USES,
        "exactly max_uses redemptions must succeed"
    );

    let reloaded = store.find_invite(&issued.row.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.uses, MAX_USES,
        "uses must never exceed max_uses, even under a concurrent burst"
    );
}

pub async fn revoked_invite_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(InviteMode::Open, &[], 1, None, "admin-1")
        .await
        .unwrap();
    assert!(store.revoke_invite(&issued.row.id).await.unwrap());

    let err = svc
        .redeem_invite(&issued.secret, "invites-open-revoked")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    let reloaded = store.find_invite(&issued.row.id).await.unwrap().unwrap();
    assert_eq!(reloaded.uses, 0);
}

pub async fn expired_invite_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let issued = svc
        .create_invite(
            InviteMode::Open,
            &[],
            1,
            Some(rfc3339_from_now(-3600)),
            "admin-1",
        )
        .await
        .unwrap();

    let err = svc
        .redeem_invite(&issued.secret, "invites-open-expired")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    let reloaded = store.find_invite(&issued.row.id).await.unwrap().unwrap();
    assert_eq!(reloaded.uses, 0);
}

pub async fn duplicate_requested_name_refused_without_consuming_a_use<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    svc.create_user("invites-open-taken-name", crate::auth::Role::Member)
        .await
        .unwrap();
    let issued = svc
        .create_invite(InviteMode::Open, &[], 1, None, "admin-1")
        .await
        .unwrap();

    let err = svc
        .redeem_invite(&issued.secret, "invites-open-taken-name")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
    let reloaded = store.find_invite(&issued.row.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.uses, 0,
        "a failed redemption must not consume a use"
    );

    // The invite is still fully usable under a different name.
    svc.redeem_invite(&issued.secret, "invites-open-free-name")
        .await
        .unwrap();
}
