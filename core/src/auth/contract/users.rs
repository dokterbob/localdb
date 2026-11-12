//! User lifecycle cases: creation, deletion cascades, and the last-admin
//! lockout guard — shared by every `AuthStore` implementation.

use std::sync::Arc;

use crate::auth::{generate_pkce_pair, AuthService, AuthStore, Role};
use crate::types::StoreVisibility;
use crate::Error;

pub async fn create_user<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = svc.create_user("users-create", Role::Member).await.unwrap();
    assert_eq!(user.name, "users-create");
    assert_eq!(user.role, Role::Member);
}

pub async fn duplicate_name_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    svc.create_user("users-dup", Role::Member).await.unwrap();
    let err = svc.create_user("users-dup", Role::Admin).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

/// Deleting a user must cascade its tokens, store grants, and OAuth
/// authorization codes — `AuthService::delete_user`'s own doc comment calls
/// out the token/grant cascade, and the schema enforces all three via
/// `ON DELETE CASCADE` on `auth_tokens.user_id`, `store_grants.user_id` and
/// `auth_codes.user_id` alike.
pub async fn delete_cascades_tokens_grants_and_auth_codes<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let admin = svc
        .create_user("users-cascade-admin", Role::Admin)
        .await
        .unwrap();
    let doomed = svc
        .create_user("users-cascade-victim", Role::Member)
        .await
        .unwrap();

    svc.issue_api_key(&doomed.id).await.unwrap();
    svc.grant_store(
        crate::auth::contract::SHARED_STORE_NAME,
        StoreVisibility::Shared,
        &doomed.id,
        &admin.id,
    )
    .await
    .unwrap();
    let (_verifier, challenge) = generate_pkce_pair();
    let issued_code = svc
        .issue_auth_code(
            crate::auth::LOCALDB_CLI_CLIENT_ID,
            &doomed.id,
            "http://127.0.0.1:1/cb",
            &challenge,
            None,
        )
        .await
        .unwrap();

    assert!(svc.delete_user(&doomed.id).await.unwrap());

    assert!(store
        .list_tokens_for_user(&doomed.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .list_grants_for_user(&doomed.id)
        .await
        .unwrap()
        .is_empty());
    assert!(
        store
            .find_auth_code_by_hash(&issued_code.row.code_hash)
            .await
            .unwrap()
            .is_none(),
        "an auth code issued to a deleted user must not survive the delete"
    );
}

pub async fn last_admin_delete_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let admin = svc
        .create_user("users-last-admin-delete", Role::Admin)
        .await
        .unwrap();
    let err = svc.delete_user(&admin.id).await.unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

pub async fn last_admin_demotion_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let admin = svc
        .create_user("users-last-admin-demote", Role::Admin)
        .await
        .unwrap();
    let err = svc
        .set_user_role(&admin.id, Role::Member)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

pub async fn demoting_a_member_is_idempotent_success<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    svc.create_user("users-demote-admin", Role::Admin)
        .await
        .unwrap();
    let member = svc
        .create_user("users-demote-member", Role::Member)
        .await
        .unwrap();
    // Already a member — demoting again is a no-op success, not an error.
    svc.set_user_role(&member.id, Role::Member).await.unwrap();
}

pub async fn demoting_unknown_id_reports_not_found<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let err = svc
        .set_user_role("users-unknown-id", Role::Member)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest { .. }));
}

pub async fn two_admins_demote_and_delete_succeed<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());

    // Deletion phase, with exactly two admins: deleting one must succeed
    // and leave exactly one admin — not zero, not still two.
    let b1 = svc
        .create_user("users-two-admins-delete-b1", Role::Admin)
        .await
        .unwrap();
    let b2 = svc
        .create_user("users-two-admins-delete-b2", Role::Admin)
        .await
        .unwrap();
    assert!(svc.delete_user(&b1.id).await.unwrap());
    let admins_remaining = store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .filter(|u| u.role == Role::Admin)
        .count();
    assert_eq!(
        admins_remaining, 1,
        "deleting one of exactly two admins must leave exactly one"
    );

    // `b2` is now the sole admin: both guards must refuse it.
    assert!(svc.delete_user(&b2.id).await.is_err());
    assert!(svc.set_user_role(&b2.id, Role::Member).await.is_err());

    // Demotion phase, with a fresh pair of exactly two admins (`b2` plus a
    // newly promoted one): demoting one must succeed, read back as a
    // member, and leave the other still an admin.
    let a1 = svc
        .create_user("users-two-admins-demote-a1", Role::Admin)
        .await
        .unwrap();
    svc.set_user_role(&a1.id, Role::Member).await.unwrap();
    assert_eq!(
        store.get_user(&a1.id).await.unwrap().unwrap().role,
        Role::Member
    );
    assert_eq!(
        store.get_user(&b2.id).await.unwrap().unwrap().role,
        Role::Admin
    );
}
