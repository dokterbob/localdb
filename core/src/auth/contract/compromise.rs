//! Transitive compromise-closure cases: a chain more than one level deep.
//!
//! `tokens::session_derived_key_revoked_with_session` already proves one
//! level of descent (a key minted under a session). This proves the
//! closure keeps descending through a second level — a session started by
//! authorizing with that key, and a further key minted under THAT session —
//! and that reuse detection on the root still reaches every descendant.

use std::sync::Arc;

use crate::auth::{
    mint_secret, rfc3339_from_now, AuthService, AuthStore, AuthTokenRow, Role, TokenKind,
};
use crate::ids::new_ulid;
use crate::Error;

async fn make_user<S: AuthStore>(svc: &AuthService<S>, name: &str) -> crate::auth::UserRow {
    svc.create_user(name, Role::Admin).await.unwrap()
}

/// Root session → API key K1 minted under it → a child session started by
/// authorizing with K1 (its `minted_by_family` is the root family) → API
/// key K2 minted under the child session. Reuse of the root's OLD refresh
/// token (after it has legitimately rotated once) must revoke every link
/// in that chain, refuse a fresh insert naming the child family, and leave
/// an unrelated session plus its own key untouched.
pub async fn reuse_on_root_revokes_two_levels_of_descendants<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let user = make_user(&svc, "compromise-transitive-user").await;

    let (root_access, root_refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let root_family = root_refresh.row.family_id.clone().unwrap();
    let k1 = svc
        .issue_api_key_minted_by(&user.id, Some(root_family.clone()))
        .await
        .unwrap();

    // The child session's ancestry is the root family, modeling what
    // `/authorize` + `/token` would record for an authorization performed
    // with K1 as the bearer credential.
    let (child_access, child_refresh) = svc
        .issue_session(&user.id, Some(root_family.clone()))
        .await
        .unwrap();
    let child_family = child_refresh.row.family_id.clone().unwrap();
    let k2 = svc
        .issue_api_key_minted_by(&user.id, Some(child_family.clone()))
        .await
        .unwrap();

    // Negative control: an entirely unrelated session and key must survive
    // the compromise sweep below untouched.
    let other = make_user(&svc, "compromise-transitive-unrelated").await;
    let (other_access, _other_refresh) = svc.issue_session(&other.id, None).await.unwrap();
    let other_key = svc.issue_api_key(&other.id).await.unwrap();

    // Legitimate rotation first — the old root refresh secret is still
    // "known" (e.g. to an attacker who captured it earlier) but no longer
    // valid for ordinary use.
    let (new_root_access, new_root_refresh) = svc
        .rotate_refresh_token(&root_refresh.secret)
        .await
        .unwrap();

    // Reuse of the now-stale root refresh secret is theft: the whole
    // closure must be revoked.
    let err = svc
        .rotate_refresh_token(&root_refresh.secret)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    assert!(svc.authenticate(&root_access.secret).await.is_err());
    assert!(svc.authenticate(&new_root_access.secret).await.is_err());
    assert!(svc
        .rotate_refresh_token(&new_root_refresh.secret)
        .await
        .is_err());
    assert!(
        svc.authenticate(&k1.secret).await.is_err(),
        "a key minted under the compromised root session must be revoked"
    );
    assert!(
        svc.authenticate(&child_access.secret).await.is_err(),
        "a session started by authorizing with a revoked key must be revoked too"
    );
    assert!(svc
        .rotate_refresh_token(&child_refresh.secret)
        .await
        .is_err());
    assert!(
        svc.authenticate(&k2.secret).await.is_err(),
        "a key minted under the compromised child session must be revoked"
    );

    // Inserting a fresh token naming the child family must be refused —
    // the compromise record outlives the sweep that created it.
    let minted = mint_secret();
    let fresh = AuthTokenRow {
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
        minted_by_family: Some(child_family),
    };
    let err = store.insert_token(&fresh).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    // The unrelated session and key are untouched.
    assert!(svc.authenticate(&other_access.secret).await.is_ok());
    assert!(svc.authenticate(&other_key.secret).await.is_ok());
}

/// `revoke_compromised_family`'s closure must be computed to a fixed
/// point, not a single pass over however the store happens to iterate its
/// rows: build a four-link family chain (root → mid → leaf → grandleaf)
/// and insert the rows in reverse-dependency order (grandleaf, then leaf,
/// then mid, then root). Under that order, a single non-recursive pass
/// adds `mid` to the closure (root is seeded before the pass starts) but
/// never revisits `leaf` — which it already checked, and rejected, before
/// `mid` was added — so `grandleaf`'s own link (`minted_by_family =
/// leaf_family`, never added to the closure at all) is missed entirely. A
/// correct transitive-closure implementation (a recursive CTE, or a
/// fixed-point loop) reaches `grandleaf` regardless of this ordering.
pub async fn revoke_compromised_family_direct_reaches_closure_regardless_of_row_order<
    S: AuthStore,
>(
    store: Arc<S>,
) {
    let svc = AuthService::new(store.clone());
    let user = make_user(&svc, "compromise-order-independent-user").await;

    let root_family = new_ulid();
    let mid_family = new_ulid();
    let leaf_family = new_ulid();
    let grandleaf_family = new_ulid();

    let link = |family: &str, minted_by: &str| AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::Refresh,
        secret_hash: mint_secret().hash,
        expires_at: Some(rfc3339_from_now(3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: Some(family.to_string()),
        rotated_from: None,
        minted_by_family: Some(minted_by.to_string()),
    };
    let root = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::Refresh,
        secret_hash: mint_secret().hash,
        expires_at: Some(rfc3339_from_now(3600)),
        last_used_at: None,
        revoked_at: None,
        created_at: rfc3339_from_now(0),
        family_id: Some(root_family.clone()),
        rotated_from: None,
        minted_by_family: None,
    };
    let mid = link(&mid_family, &root_family);
    let leaf = link(&leaf_family, &mid_family);
    let grandleaf = link(&grandleaf_family, &leaf_family);

    // Deliberately adversarial order: each row before the one it depends
    // on, ending with `root` itself last.
    store.insert_token(&grandleaf).await.unwrap();
    store.insert_token(&leaf).await.unwrap();
    store.insert_token(&mid).await.unwrap();
    store.insert_token(&root).await.unwrap();

    store.revoke_compromised_family(&root_family).await.unwrap();

    assert!(store
        .find_token(&root.id)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(store
        .find_token(&mid.id)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(store
        .find_token(&leaf.id)
        .await
        .unwrap()
        .unwrap()
        .revoked_at
        .is_some());
    assert!(
        store
            .find_token(&grandleaf.id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some(),
        "the fourth-level family must be reached regardless of insertion order"
    );
}
