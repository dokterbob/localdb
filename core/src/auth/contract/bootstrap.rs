//! First-admin bootstrap cases shared by every `AuthStore` implementation.

use std::sync::Arc;

use crate::auth::{AuthService, AuthStore, Principal, Role, StoreAccess};

pub async fn first_admin_gets_admin_name<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let admin = svc.begin_bootstrap().await.unwrap();
    assert_eq!(admin.name, "admin");
    assert_eq!(admin.role, Role::Admin);
}

pub async fn bootstrap_admin_gets_next_free_name_when_admin_taken<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    svc.create_user("admin", Role::Member).await.unwrap();
    let admin = svc.begin_bootstrap().await.unwrap();
    assert_eq!(admin.name, "admin-2");
}

pub async fn bootstrap_needed_before_and_after<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    assert!(svc.bootstrap_needed().await.unwrap());

    let pending = svc.begin_bootstrap().await.unwrap();
    // Begun but not yet completed: still needed.
    assert!(svc.bootstrap_needed().await.unwrap());

    let principal = Principal {
        user_id: pending.id.clone(),
        name: pending.name.clone(),
        role: Role::Admin,
        access: StoreAccess::All,
    };
    svc.complete_bootstrap(&principal).await.unwrap();
    assert!(!svc.bootstrap_needed().await.unwrap());
}

/// `complete_bootstrap` only clears the pending marker for an admin
/// principal — `AuthService::complete_bootstrap`'s own guard
/// (`if principal.role == Role::Admin`) means a non-admin caller touching
/// this path (which should not normally happen) must leave it untouched.
pub async fn complete_bootstrap_semantics<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let pending = svc.begin_bootstrap().await.unwrap();

    let member_principal = Principal {
        user_id: pending.id.clone(),
        name: pending.name.clone(),
        role: Role::Member,
        access: StoreAccess::Granted(Default::default()),
    };
    svc.complete_bootstrap(&member_principal).await.unwrap();
    assert!(
        store.pending_bootstrap_user().await.unwrap().is_some(),
        "a non-admin principal must not clear the pending bootstrap marker"
    );

    let admin_principal = Principal {
        user_id: pending.id.clone(),
        name: pending.name.clone(),
        role: Role::Admin,
        access: StoreAccess::All,
    };
    svc.complete_bootstrap(&admin_principal).await.unwrap();
    assert!(store.pending_bootstrap_user().await.unwrap().is_none());
}
