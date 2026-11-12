//! Store-grant cases shared by every `AuthStore` implementation.
//!
//! Cases that actually persist a grant use [`super::SHARED_STORE_NAME`] —
//! `LibsqlAuthStore`'s `store_grants.store_name` has a `REFERENCES
//! stores(name)` FK, so a caller instantiating this suite against it must
//! pre-provision a `stores` row under that exact name.

use std::sync::Arc;

use crate::auth::{AuthService, AuthStore, Role};
use crate::types::StoreVisibility;
use crate::Error;

pub async fn grant_list_and_revoke<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let admin = svc.create_user("grants-admin", Role::Admin).await.unwrap();
    let member = svc
        .create_user("grants-member", Role::Member)
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

    let principal = svc
        .authenticate(&svc.issue_api_key(&member.id).await.unwrap().secret)
        .await
        .unwrap();
    assert!(principal.can_read_store(super::SHARED_STORE_NAME, StoreVisibility::Shared));

    assert!(svc
        .revoke_store(super::SHARED_STORE_NAME, &member.id)
        .await
        .unwrap());
    // A second revoke of the same grant reports nothing to remove.
    assert!(!svc
        .revoke_store(super::SHARED_STORE_NAME, &member.id)
        .await
        .unwrap());
}

/// `AuthService::grant_store` rejects a grant against a `private` store
/// purely on the `visibility` the caller passes in — no store row needs to
/// exist for this case either way.
pub async fn grant_on_private_store_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let admin = svc
        .create_user("grants-private-admin", Role::Admin)
        .await
        .unwrap();
    let member = svc
        .create_user("grants-private-member", Role::Member)
        .await
        .unwrap();

    let err = svc
        .grant_store(
            "private-store",
            StoreVisibility::Private,
            &member.id,
            &admin.id,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Forbidden { .. }));
}

pub async fn deleting_user_removes_grants<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let admin = svc
        .create_user("grants-delete-admin", Role::Admin)
        .await
        .unwrap();
    let member = svc
        .create_user("grants-delete-member", Role::Member)
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

    assert!(svc.delete_user(&member.id).await.unwrap());
    assert!(store
        .list_grants_for_user(&member.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .list_grants_for_store(super::SHARED_STORE_NAME)
        .await
        .unwrap()
        .is_empty());
}
