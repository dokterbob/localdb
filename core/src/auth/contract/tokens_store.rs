//! Direct `AuthStore::rotate_tokens` cases, plus coverage for the token
//! methods `AuthService` never exercises on its own (`mark_token_used`,
//! `list_tokens_for_user`, `revoke_token_family`).
//!
//! `AuthService::rotate_refresh_token` rejects an unknown, wrong-kind,
//! expired, or already-revoked token before ever calling
//! `AuthStore::rotate_tokens` — so driving these cases through the service
//! would never reach the store's own guard at all. Each case here calls
//! `rotate_tokens` directly, building the replacement rows the same way
//! `AuthService::prepare_token`/`rotate_refresh_token` do.

use std::sync::Arc;

use crate::auth::{
    mint_secret, rfc3339_from_now, AuthStore, AuthTokenRow, Role, RotateOutcome, TokenKind,
};
use crate::ids::new_ulid;

async fn seed_user<S: AuthStore>(store: &S, name: &str) -> crate::auth::UserRow {
    let user = crate::auth::UserRow {
        id: new_ulid(),
        name: name.to_string(),
        role: Role::Member,
        created_at: rfc3339_from_now(0),
    };
    store.create_user(&user).await.unwrap();
    user
}

/// A refresh token row, live by default — override via the closure for the
/// expired/revoked variants each case needs.
fn refresh_row(
    user_id: &str,
    family: &str,
    mutate: impl FnOnce(&mut AuthTokenRow),
) -> AuthTokenRow {
    let minted = mint_secret();
    let mut row = AuthTokenRow {
        id: new_ulid(),
        user_id: user_id.to_string(),
        kind: TokenKind::Refresh,
        secret_hash: minted.hash,
        expires_at: Some(rfc3339_from_now(3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(-10),
        family_id: Some(family.to_string()),
        rotated_from: None,
        minted_by_family: None,
    };
    mutate(&mut row);
    row
}

/// The replacement access/refresh pair `rotate_tokens` would insert for
/// `old`, mirroring `AuthService::rotate_refresh_token`'s own construction.
fn replacements(old: &AuthTokenRow) -> (AuthTokenRow, AuthTokenRow) {
    let family = old.family_id.clone().unwrap();
    let new_refresh = AuthTokenRow {
        id: new_ulid(),
        user_id: old.user_id.clone(),
        kind: TokenKind::Refresh,
        secret_hash: mint_secret().hash,
        expires_at: Some(rfc3339_from_now(3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: Some(family.clone()),
        rotated_from: Some(old.id.clone()),
        minted_by_family: old.minted_by_family.clone(),
    };
    let new_access = AuthTokenRow {
        id: new_ulid(),
        user_id: old.user_id.clone(),
        kind: TokenKind::Access,
        secret_hash: mint_secret().hash,
        expires_at: Some(rfc3339_from_now(3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: Some(family),
        rotated_from: None,
        minted_by_family: old.minted_by_family.clone(),
    };
    (new_access, new_refresh)
}

pub async fn rotate_live_refresh_token_direct<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-rotate-live").await;
    let family = new_ulid();
    let old = refresh_row(&user.id, &family, |_| {});
    store.insert_token(&old).await.unwrap();
    let (access, refresh) = replacements(&old);

    let outcome = store
        .rotate_tokens(&old.id, &access, &refresh)
        .await
        .unwrap();
    assert_eq!(outcome, RotateOutcome::Rotated);

    let reloaded_old = store.find_token(&old.id).await.unwrap().unwrap();
    assert!(
        reloaded_old.revoked_at.is_some(),
        "the rotated-away refresh token must be revoked"
    );
    assert!(
        store.find_token(&access.id).await.unwrap().is_some(),
        "the replacement access token must be persisted"
    );
    assert!(
        store.find_token(&refresh.id).await.unwrap().is_some(),
        "the replacement refresh token must be persisted"
    );
}

pub async fn rotate_already_rotated_refresh_token_direct<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-rotate-already").await;
    let family = new_ulid();
    let old = refresh_row(&user.id, &family, |r| {
        r.revoked_at = Some(rfc3339_from_now(-10));
    });
    store.insert_token(&old).await.unwrap();
    let (access, refresh) = replacements(&old);

    let outcome = store
        .rotate_tokens(&old.id, &access, &refresh)
        .await
        .unwrap();
    assert_eq!(outcome, RotateOutcome::AlreadyRotated);

    assert!(
        store.find_token(&access.id).await.unwrap().is_none(),
        "a non-Rotated outcome must not insert the replacement access token"
    );
    assert!(
        store.find_token(&refresh.id).await.unwrap().is_none(),
        "a non-Rotated outcome must not insert the replacement refresh token"
    );
    let reloaded_old = store.find_token(&old.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded_old.revoked_at, old.revoked_at,
        "the old row's revocation must not change on an AlreadyRotated outcome"
    );
}

pub async fn rotate_expired_refresh_token_direct<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-rotate-expired").await;
    let family = new_ulid();
    let old = refresh_row(&user.id, &family, |r| {
        r.expires_at = Some(rfc3339_from_now(-3600));
    });
    store.insert_token(&old).await.unwrap();
    let (access, refresh) = replacements(&old);

    let outcome = store
        .rotate_tokens(&old.id, &access, &refresh)
        .await
        .unwrap();
    assert_eq!(outcome, RotateOutcome::Ineligible);

    assert!(store.find_token(&access.id).await.unwrap().is_none());
    assert!(store.find_token(&refresh.id).await.unwrap().is_none());
    let reloaded_old = store.find_token(&old.id).await.unwrap().unwrap();
    assert!(reloaded_old.revoked_at.is_none());
}

pub async fn rotate_wrong_kind_access_token_with_family_direct<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-rotate-access-kind").await;
    let family = new_ulid();
    let minted = mint_secret();
    let old = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::Access,
        secret_hash: minted.hash,
        expires_at: Some(rfc3339_from_now(3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: Some(family.clone()),
        rotated_from: None,
        minted_by_family: None,
    };
    store.insert_token(&old).await.unwrap();
    let (access, refresh) = replacements(&old);

    let outcome = store
        .rotate_tokens(&old.id, &access, &refresh)
        .await
        .unwrap();
    assert_eq!(outcome, RotateOutcome::Ineligible);
    assert!(store.find_token(&access.id).await.unwrap().is_none());
    assert!(store.find_token(&refresh.id).await.unwrap().is_none());
}

pub async fn rotate_wrong_kind_api_key_direct<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-rotate-api-key-kind").await;
    let minted = mint_secret();
    let old = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::ApiKey,
        secret_hash: minted.hash,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    };
    store.insert_token(&old).await.unwrap();
    // An API key carries no `family_id`, so the replacements need one of
    // their own rather than borrowing `old`'s (there is none).
    let family = new_ulid();
    let access = refresh_row(&user.id, &family, |r| r.kind = TokenKind::Access);
    let refresh = refresh_row(&user.id, &family, |_| {});

    let outcome = store
        .rotate_tokens(&old.id, &access, &refresh)
        .await
        .unwrap();
    assert_eq!(outcome, RotateOutcome::Ineligible);
    assert!(store.find_token(&access.id).await.unwrap().is_none());
    assert!(store.find_token(&refresh.id).await.unwrap().is_none());
}

pub async fn rotate_unknown_id_direct<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-rotate-unknown").await;
    let family = new_ulid();
    let access = refresh_row(&user.id, &family, |r| r.kind = TokenKind::Access);
    let refresh = refresh_row(&user.id, &family, |_| {});

    let outcome = store
        .rotate_tokens(&new_ulid(), &access, &refresh)
        .await
        .unwrap();
    assert_eq!(outcome, RotateOutcome::Ineligible);
    assert!(store.find_token(&access.id).await.unwrap().is_none());
    assert!(store.find_token(&refresh.id).await.unwrap().is_none());
}

/// Deterministic concurrency check: a burst of simultaneous rotations
/// presenting the SAME live refresh token must yield exactly one `Rotated`
/// outcome — the others must each observe `AlreadyRotated` (the second
/// racer onward always finds the row already revoked by the winner).
pub async fn concurrent_rotate_same_refresh_token_yields_one_rotation<S: AuthStore>(store: Arc<S>) {
    const CONTENDERS: usize = 8;
    let user = seed_user(&*store, "tokens-store-rotate-concurrent").await;
    let family = new_ulid();
    let old = refresh_row(&user.id, &family, |_| {});
    store.insert_token(&old).await.unwrap();

    let barrier = Arc::new(tokio::sync::Barrier::new(CONTENDERS));
    let mut handles = Vec::new();
    for _ in 0..CONTENDERS {
        let store = store.clone();
        let old = old.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            let (access, refresh) = replacements(&old);
            barrier.wait().await;
            store.rotate_tokens(&old.id, &access, &refresh).await
        }));
    }
    let mut rotated = 0u32;
    let mut already_rotated = 0u32;
    for h in handles {
        match h.await.unwrap().unwrap() {
            RotateOutcome::Rotated => rotated += 1,
            RotateOutcome::AlreadyRotated => already_rotated += 1,
            other => panic!("unexpected rotation outcome under a concurrent burst: {other:?}"),
        }
    }
    assert_eq!(rotated, 1, "exactly one concurrent rotation must win");
    assert_eq!(already_rotated, CONTENDERS as u32 - 1);
}

/// `mark_token_used` must set `last_used_at` and leave everything else
/// untouched — `AuthService::authenticate_with_origin` calls it on every
/// successful API-key authentication, but no contract case reads the field
/// back directly.
pub async fn mark_token_used_sets_last_used_at<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-mark-used").await;
    let minted = mint_secret();
    let row = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::ApiKey,
        secret_hash: minted.hash,
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: None,
        rotated_from: None,
        minted_by_family: None,
    };
    store.insert_token(&row).await.unwrap();
    assert!(store
        .find_token(&row.id)
        .await
        .unwrap()
        .unwrap()
        .last_used_at
        .is_none());

    let used_at = rfc3339_from_now(0);
    store.mark_token_used(&row.id, &used_at).await.unwrap();
    let reloaded = store.find_token(&row.id).await.unwrap().unwrap();
    assert_eq!(reloaded.last_used_at, Some(used_at));
}

/// `list_tokens_for_user` must return every token minted for that user,
/// and none minted for anyone else.
pub async fn list_tokens_for_user_returns_only_that_users_tokens<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-list-mine").await;
    let other = seed_user(&*store, "tokens-store-list-other").await;
    let mine_1 = AuthTokenRow {
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
    let mine_2 = AuthTokenRow {
        id: new_ulid(),
        secret_hash: mint_secret().hash,
        ..mine_1.clone()
    };
    let theirs = AuthTokenRow {
        id: new_ulid(),
        user_id: other.id.clone(),
        secret_hash: mint_secret().hash,
        ..mine_1.clone()
    };
    store.insert_token(&mine_1).await.unwrap();
    store.insert_token(&mine_2).await.unwrap();
    store.insert_token(&theirs).await.unwrap();

    let listed = store.list_tokens_for_user(&user.id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|t| t.user_id == user.id));
    assert!(listed.iter().any(|t| t.id == mine_1.id));
    assert!(listed.iter().any(|t| t.id == mine_2.id));
}

/// `revoke_token_family` must revoke every unrevoked token sharing
/// `family_id` (both the access and the refresh token), report the count
/// revoked, and leave a different family untouched.
pub async fn revoke_token_family_revokes_every_member<S: AuthStore>(store: Arc<S>) {
    let user = seed_user(&*store, "tokens-store-revoke-family").await;
    let family = new_ulid();
    let access = refresh_row(&user.id, &family, |r| r.kind = TokenKind::Access);
    let refresh = refresh_row(&user.id, &family, |_| {});
    let other_family = new_ulid();
    let unrelated = refresh_row(&user.id, &other_family, |_| {});
    store.insert_token(&access).await.unwrap();
    store.insert_token(&refresh).await.unwrap();
    store.insert_token(&unrelated).await.unwrap();

    let revoked = store.revoke_token_family(&family).await.unwrap();
    assert_eq!(revoked, 2);
    assert!(store
        .find_token(&access.id)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(store
        .find_token(&refresh.id)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(
        store
            .find_token(&unrelated.id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_none(),
        "a different family must be untouched"
    );
}
