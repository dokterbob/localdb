//! Token issuance, authentication, rotation and compromise-closure cases
//! shared by every `AuthStore` implementation.
//!
//! Expiry boundaries are tested with clearly-past/clearly-future timestamps
//! built via `rfc3339_from_now`, never the exact-second boundary — the
//! trait gives callers no way to supply "now" to `rotate_tokens`, so pinning
//! the boundary itself is left to the libsql-only `rotate_tokens_at` tests
//! (`store-libsql/src/auth/tests/atomic.rs`), which can.

use std::sync::Arc;

use crate::auth::{
    mint_secret, rfc3339_from_now, AuthService, AuthStore, AuthTokenRow, Role, TokenKind,
};
use crate::ids::new_ulid;
use crate::Error;

async fn make_user<S: AuthStore>(svc: &AuthService<S>, name: &str) -> crate::auth::UserRow {
    svc.create_user(name, Role::Admin).await.unwrap()
}

pub async fn issue_and_authenticate_api_key<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-api-key").await;
    let issued = svc.issue_api_key(&user.id).await.unwrap();

    let principal = svc.authenticate(&issued.secret).await.unwrap();
    assert_eq!(principal.user_id, user.id);
    // A wrong secret must never authenticate.
    assert!(svc.authenticate("ldb_not-the-real-secret").await.is_err());
}

pub async fn issue_and_authenticate_access_token<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-access-token").await;
    let issued = svc.issue_access_token(&user.id).await.unwrap();

    let principal = svc.authenticate(&issued.secret).await.unwrap();
    assert_eq!(principal.user_id, user.id);
}

pub async fn revoked_token_rejected<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-revoked").await;
    let issued = svc.issue_api_key(&user.id).await.unwrap();
    assert!(svc.revoke_by_secret(&issued.secret).await.unwrap());

    let err = svc.authenticate(&issued.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

pub async fn expired_token_rejected<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let user = make_user(&svc, "tokens-expired").await;
    let minted = mint_secret();
    let row = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::Access,
        secret_hash: minted.hash.clone(),
        expires_at: Some(rfc3339_from_now(-3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(-3600),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    };
    store.insert_token(&row).await.unwrap();

    let err = svc.authenticate(&minted.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

pub async fn refresh_rotation_happy_path<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-rotation-happy").await;
    let refresh = svc.issue_refresh_token(&user.id).await.unwrap();

    let (new_access, new_refresh) = svc.rotate_refresh_token(&refresh.secret).await.unwrap();
    assert_eq!(new_refresh.row.family_id, refresh.row.family_id);
    assert_eq!(new_refresh.row.rotated_from, Some(refresh.row.id.clone()));

    // The new pair is fully usable: the new access token authenticates and
    // the new refresh token itself rotates again. (Re-presenting the OLD
    // refresh secret a second time is reuse detection, covered by its own
    // case below — doing it here would burn this very family.)
    assert!(svc.authenticate(&new_access.secret).await.is_ok());
    assert!(svc.rotate_refresh_token(&new_refresh.secret).await.is_ok());
}

pub async fn rotating_already_rotated_unexpired_token_revokes_family<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-reuse").await;
    let refresh = svc.issue_refresh_token(&user.id).await.unwrap();
    let (rotated_access, rotated_refresh) =
        svc.rotate_refresh_token(&refresh.secret).await.unwrap();

    // Reuse of the now-rotated-away refresh secret is theft: the whole
    // family — including the freshly minted replacement pair — is revoked.
    let err = svc.rotate_refresh_token(&refresh.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    assert!(
        svc.authenticate(&rotated_access.secret).await.is_err(),
        "the replacement access token must be revoked too"
    );
    assert!(svc
        .rotate_refresh_token(&rotated_refresh.secret)
        .await
        .is_err());
}

pub async fn rotating_unknown_token_fails_without_revocation<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-unknown-rotate").await;
    // A live, unrelated session must stay completely unaffected by an
    // unknown-token rotation attempt — otherwise this case would pass
    // against a store that revoked something on every rotation call,
    // known or not.
    let (live_access, live_refresh) = svc.issue_session(&user.id, None).await.unwrap();

    let err = svc.rotate_refresh_token("ldb_totally-unknown-secret").await;
    assert!(matches!(err.unwrap_err(), Error::Unauthorized { .. }));

    assert!(svc.authenticate(&live_access.secret).await.is_ok());
    assert!(svc.rotate_refresh_token(&live_refresh.secret).await.is_ok());
}

pub async fn rotating_wrong_kind_token_fails_without_revocation<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-wrong-kind").await;
    // A family-bearing access token (minted via `issue_session`, not the
    // family-less `issue_access_token`) so this case also proves the wrong
    // kind check doesn't key off "has a family_id" — the refresh token
    // sharing that same family must still rotate normally afterwards.
    let (access, refresh) = svc.issue_session(&user.id, None).await.unwrap();

    let err = svc.rotate_refresh_token(&access.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    // Presenting the wrong kind is not reuse evidence: the access token
    // itself must remain perfectly usable as a bearer credential, and the
    // family's refresh token must still rotate normally afterwards.
    assert!(svc.authenticate(&access.secret).await.is_ok());
    assert!(svc.rotate_refresh_token(&refresh.secret).await.is_ok());
}

pub async fn rotating_expired_token_fails_without_revocation<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let user = make_user(&svc, "tokens-expired-rotate").await;
    let minted = mint_secret();
    let family = new_ulid();
    let expired = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::Refresh,
        secret_hash: minted.hash.clone(),
        expires_at: Some(rfc3339_from_now(-3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(-7200),
        family_id: Some(family.clone()),
        rotated_from: None,
        minted_by_family: None,
    };
    store.insert_token(&expired).await.unwrap();

    let err = svc.rotate_refresh_token(&minted.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    // A plain expiry is not reuse evidence: nothing in the family (there is
    // only this one row) was revoked by the attempt.
    let reloaded = store.find_token(&expired.id).await.unwrap().unwrap();
    assert!(
        reloaded.revoked_at.is_none(),
        "an expired-only rotation attempt must not revoke the token itself"
    );
}

pub async fn rotating_expired_and_rotated_token_fails_without_revocation<S: AuthStore>(
    store: Arc<S>,
) {
    let svc = AuthService::new(store.clone());
    let user = make_user(&svc, "tokens-expired-and-rotated").await;
    // A live session shares the family with the stale, already-rotated AND
    // expired row below — reuse of the stale row must leave this live
    // session completely untouched.
    let (live_access, live_refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = live_refresh.row.family_id.clone().unwrap();

    let minted = mint_secret();
    let stale = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::Refresh,
        secret_hash: minted.hash.clone(),
        expires_at: Some(rfc3339_from_now(-3600)),
        last_used_at: None,
        revoked_at: Some(rfc3339_from_now(-1800)),
        created_at: rfc3339_from_now(-7200),
        family_id: Some(family),
        rotated_from: None,
        minted_by_family: None,
    };
    store.insert_token(&stale).await.unwrap();

    let err = svc.rotate_refresh_token(&minted.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    assert!(svc.authenticate(&live_access.secret).await.is_ok());
    assert!(svc.rotate_refresh_token(&live_refresh.secret).await.is_ok());
}

/// `AuthStore::insert_token` must refuse a token naming an already-
/// compromised family with the identical `Result` shape in every
/// implementation — an `Ok(false)` in one and an `Err` in the other would
/// make `AuthService` (which treats this as `Err`) behave inconsistently
/// across stores.
pub async fn insert_after_family_compromised_refused_identically<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    // A real, persisted user so a refusal here is attributable to the
    // compromise guard alone, never to an unrelated foreign-key failure
    // over a random, unknown `user_id`.
    let user = make_user(&svc, "tokens-compromised-insert-user").await;
    let family = new_ulid();
    store.revoke_compromised_family(&family).await.unwrap();

    let minted = mint_secret();
    let row = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id,
        kind: TokenKind::ApiKey,
        secret_hash: minted.hash.clone(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: Some(family),
    };
    let err = store.insert_token(&row).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    assert!(
        store
            .find_token_by_hash(&minted.hash)
            .await
            .unwrap()
            .is_none(),
        "a refused insert must not have persisted anything"
    );
}

/// A key minted on behalf of a session (`issue_api_key_minted_by`) is part
/// of that session's compromise closure: reuse detection on the session's
/// own refresh token must revoke the key too, not just the session's own
/// access/refresh tokens.
pub async fn session_derived_key_revoked_with_session<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-session-ancestry").await;
    let (_access, refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = refresh.row.family_id.clone().unwrap();
    let key = svc
        .issue_api_key_minted_by(&user.id, Some(family))
        .await
        .unwrap();
    assert!(svc.authenticate(&key.secret).await.is_ok());

    svc.rotate_refresh_token(&refresh.secret).await.unwrap();
    let err = svc.rotate_refresh_token(&refresh.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    assert!(
        svc.authenticate(&key.secret).await.is_err(),
        "a key minted under the session must be revoked when the session is"
    );
}

/// RFC 7009 logout (`revoke_by_secret` on a refresh token) is deliberately
/// narrower than reuse-detected compromise: it ends the session's own
/// access/refresh tokens but must leave a key minted under that same
/// session alone — the user asked for that key on purpose.
pub async fn logout_revokes_family_but_not_a_minted_key<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = make_user(&svc, "tokens-logout").await;
    let (access, refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = refresh.row.family_id.clone().unwrap();
    let key = svc
        .issue_api_key_minted_by(&user.id, Some(family))
        .await
        .unwrap();

    assert!(svc.revoke_by_secret(&refresh.secret).await.unwrap());

    assert!(svc.authenticate(&access.secret).await.is_err());
    assert!(
        svc.authenticate(&key.secret).await.is_ok(),
        "logout must not revoke a key minted under the session"
    );
}
