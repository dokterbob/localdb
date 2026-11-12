//! Direct `AuthStore::redeem_open_invite` cases for the store-level
//! eligibility guard (revoked/expired/exhausted), plus `list_invites`
//! coverage.
//!
//! `AuthService::redeem_invite` already checks revocation and expiry
//! itself before ever calling `redeem_open_invite` — an exhausted invite is
//! the only one of the three `AuthService` can't pre-filter (it has no
//! cheap way to know `uses == max_uses` without the same atomic read the
//! store performs), so only that path is reachable through the service.
//! These call the store method directly so all three guards are actually
//! exercised at the layer that enforces them.

use std::sync::Arc;

use crate::auth::{
    mint_secret, rfc3339_from_now, AuthStore, AuthTokenRow, InviteMode, InviteRow, Role, TokenKind,
    UserRow,
};
use crate::ids::new_ulid;

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

fn redemption_args(name: &str) -> (UserRow, AuthTokenRow) {
    let user = UserRow {
        id: new_ulid(),
        name: name.to_string(),
        role: Role::Member,
        created_at: rfc3339_from_now(0),
    };
    let credential = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::ApiKey,
        secret_hash: mint_secret().hash,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    };
    (user, credential)
}

pub async fn redeem_open_invite_direct_on_revoked_invite_is_refused<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Open, 5, 0, |i| {
        i.revoked_at = Some(rfc3339_from_now(0));
    });
    store.create_invite(&invite).await.unwrap();
    let (user, credential) = redemption_args("invites-open-store-revoked");

    let ok = store
        .redeem_open_invite(&invite.id, &user, &[], &credential)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store.get_user(&user.id).await.unwrap().is_none());
    let reloaded = store.find_invite(&invite.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.uses, 0,
        "a refused redemption must not consume a use"
    );
}

pub async fn redeem_open_invite_direct_on_expired_invite_is_refused<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Open, 5, 0, |i| {
        i.expires_at = Some(rfc3339_from_now(-3600));
    });
    store.create_invite(&invite).await.unwrap();
    let (user, credential) = redemption_args("invites-open-store-expired");

    let ok = store
        .redeem_open_invite(&invite.id, &user, &[], &credential)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store.get_user(&user.id).await.unwrap().is_none());
    let reloaded = store.find_invite(&invite.id).await.unwrap().unwrap();
    assert_eq!(reloaded.uses, 0);
}

pub async fn redeem_open_invite_direct_on_exhausted_invite_is_refused<S: AuthStore>(store: Arc<S>) {
    let invite = seed_invite(InviteMode::Open, 2, 2, |_| {});
    store.create_invite(&invite).await.unwrap();
    let (user, credential) = redemption_args("invites-open-store-exhausted");

    let ok = store
        .redeem_open_invite(&invite.id, &user, &[], &credential)
        .await
        .unwrap();
    assert!(!ok);
    assert!(store.get_user(&user.id).await.unwrap().is_none());
    let reloaded = store.find_invite(&invite.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.uses, 2,
        "an exhausted invite's uses must not change"
    );
}

pub async fn list_invites_returns_every_invite<S: AuthStore>(store: Arc<S>) {
    let a = seed_invite(InviteMode::Open, 1, 0, |_| {});
    let b = seed_invite(InviteMode::Closed, 1, 0, |_| {});
    store.create_invite(&a).await.unwrap();
    store.create_invite(&b).await.unwrap();

    let listed = store.list_invites().await.unwrap();
    assert!(listed.iter().any(|i| i.id == a.id));
    assert!(listed.iter().any(|i| i.id == b.id));
}
