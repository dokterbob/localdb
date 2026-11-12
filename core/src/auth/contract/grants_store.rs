//! Coverage for `AuthStore` grant methods `AuthService`'s own cases only
//! exercise incidentally: `list_grants_for_store` with real content, and
//! `revoke_store_grant` followed by a fresh authorization re-read proving
//! the access is actually gone, not merely that the store reported a
//! removal.

use std::sync::Arc;

use crate::auth::{AuthService, AuthStore, Role};
use crate::types::StoreVisibility;

pub async fn list_grants_for_store_returns_every_grantee<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let admin = svc
        .create_user("grants-store-list-admin", Role::Admin)
        .await
        .unwrap();
    let member_a = svc
        .create_user("grants-store-list-member-a", Role::Member)
        .await
        .unwrap();
    let member_b = svc
        .create_user("grants-store-list-member-b", Role::Member)
        .await
        .unwrap();
    let ungranted = svc
        .create_user("grants-store-list-ungranted", Role::Member)
        .await
        .unwrap();

    svc.grant_store(
        super::SHARED_STORE_NAME,
        StoreVisibility::Shared,
        &member_a.id,
        &admin.id,
    )
    .await
    .unwrap();
    svc.grant_store(
        super::SHARED_STORE_NAME,
        StoreVisibility::Shared,
        &member_b.id,
        &admin.id,
    )
    .await
    .unwrap();

    let listed = store
        .list_grants_for_store(super::SHARED_STORE_NAME)
        .await
        .unwrap();
    assert!(listed.iter().any(|g| g.user_id == member_a.id));
    assert!(listed.iter().any(|g| g.user_id == member_b.id));
    assert!(!listed.iter().any(|g| g.user_id == ungranted.id));
}

/// `revoke_store_grant` reporting `true` is not by itself proof that access
/// is gone — re-authenticate afterward and check `can_read_store` directly.
pub async fn revoke_store_grant_then_reauthorized_principal_loses_access<S: AuthStore>(
    store: Arc<S>,
) {
    let svc = AuthService::new(store);
    let admin = svc
        .create_user("grants-store-revoke-reread-admin", Role::Admin)
        .await
        .unwrap();
    let member = svc
        .create_user("grants-store-revoke-reread-member", Role::Member)
        .await
        .unwrap();
    svc.grant_store(
        super::SHARED_STORE_NAME,
        StoreVisibility::Shared,
        &member.id,
        &admin.id,
    )
    .await
    .unwrap();
    let key = svc.issue_api_key(&member.id).await.unwrap();

    let before = svc.authenticate(&key.secret).await.unwrap();
    assert!(before.can_read_store(super::SHARED_STORE_NAME, StoreVisibility::Shared));

    assert!(svc
        .revoke_store(super::SHARED_STORE_NAME, &member.id)
        .await
        .unwrap());

    let after = svc.authenticate(&key.secret).await.unwrap();
    assert!(
        !after.can_read_store(super::SHARED_STORE_NAME, StoreVisibility::Shared),
        "a freshly re-authenticated principal must not retain revoked access"
    );
}
