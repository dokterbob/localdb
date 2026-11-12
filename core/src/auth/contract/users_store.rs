//! Direct `AuthStore` cases for the user-lifecycle store guards
//! (`try_demote_user_unless_last_admin`, `try_delete_user_unless_last_admin`)
//! and coverage for the plain CRUD methods `AuthService` never exercises
//! directly (`update_user_role`, raw `delete_user`, `count_users`,
//! `admin_exists`).
//!
//! `AuthService::set_user_role`/`delete_user` only ever see the two
//! outcomes they map onto a public `Result` (`Ok`/`InvalidRequest`), so a
//! store that collapsed `DemoteOutcome::NotFound` into `LastAdmin`, or
//! `try_delete_user_unless_last_admin`'s `false` into the wrong cause,
//! would still pass every service-level case. These call the store methods
//! directly and assert the exact outcome.

use std::sync::Arc;

use crate::auth::{AuthStore, DemoteOutcome, Role, UserRow};
use crate::ids::new_ulid;
use crate::ingestion::now_rfc3339;

async fn seed_user<S: AuthStore>(store: &S, name: &str, role: Role) -> UserRow {
    let user = UserRow {
        id: new_ulid(),
        name: name.to_string(),
        role,
        created_at: now_rfc3339(),
    };
    store.create_user(&user).await.unwrap();
    user
}

pub async fn demote_sole_admin_direct_is_last_admin<S: AuthStore>(store: Arc<S>) {
    let admin = seed_user(&*store, "users-store-demote-sole-admin", Role::Admin).await;
    let outcome = store
        .try_demote_user_unless_last_admin(&admin.id)
        .await
        .unwrap();
    assert_eq!(outcome, DemoteOutcome::LastAdmin);
    let reloaded = store.get_user(&admin.id).await.unwrap().unwrap();
    assert_eq!(
        reloaded.role,
        Role::Admin,
        "a refused demotion must not change the role"
    );
}

pub async fn demote_unknown_id_direct_is_not_found<S: AuthStore>(store: Arc<S>) {
    let outcome = store
        .try_demote_user_unless_last_admin("users-store-unknown-demote-id")
        .await
        .unwrap();
    assert_eq!(outcome, DemoteOutcome::NotFound);
}

pub async fn demote_member_direct_is_done<S: AuthStore>(store: Arc<S>) {
    seed_user(&*store, "users-store-demote-member-admin", Role::Admin).await;
    let member = seed_user(&*store, "users-store-demote-member-target", Role::Member).await;
    let outcome = store
        .try_demote_user_unless_last_admin(&member.id)
        .await
        .unwrap();
    assert_eq!(outcome, DemoteOutcome::Done);
    let reloaded = store.get_user(&member.id).await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Member);
}

pub async fn demote_one_of_two_admins_direct_is_done<S: AuthStore>(store: Arc<S>) {
    let a1 = seed_user(&*store, "users-store-demote-two-a1", Role::Admin).await;
    let a2 = seed_user(&*store, "users-store-demote-two-a2", Role::Admin).await;
    let outcome = store
        .try_demote_user_unless_last_admin(&a1.id)
        .await
        .unwrap();
    assert_eq!(outcome, DemoteOutcome::Done);
    assert_eq!(
        store.get_user(&a1.id).await.unwrap().unwrap().role,
        Role::Member
    );
    assert_eq!(
        store.get_user(&a2.id).await.unwrap().unwrap().role,
        Role::Admin
    );
}

pub async fn delete_sole_admin_direct_is_refused<S: AuthStore>(store: Arc<S>) {
    let admin = seed_user(&*store, "users-store-delete-sole-admin", Role::Admin).await;
    let deleted = store
        .try_delete_user_unless_last_admin(&admin.id)
        .await
        .unwrap();
    assert!(!deleted);
    assert!(
        store.get_user(&admin.id).await.unwrap().is_some(),
        "a refused delete must leave the user in place"
    );
}

pub async fn delete_unknown_id_direct_is_false<S: AuthStore>(store: Arc<S>) {
    let deleted = store
        .try_delete_user_unless_last_admin("users-store-unknown-delete-id")
        .await
        .unwrap();
    assert!(!deleted);
}

pub async fn delete_one_of_two_admins_direct_leaves_one<S: AuthStore>(store: Arc<S>) {
    let a1 = seed_user(&*store, "users-store-delete-two-a1", Role::Admin).await;
    let a2 = seed_user(&*store, "users-store-delete-two-a2", Role::Admin).await;
    let deleted = store
        .try_delete_user_unless_last_admin(&a1.id)
        .await
        .unwrap();
    assert!(deleted);
    assert!(store.get_user(&a1.id).await.unwrap().is_none());
    assert!(store.get_user(&a2.id).await.unwrap().is_some());
}

/// `update_user_role` is the plain, unguarded write — `AuthService` only
/// calls it for a promotion (never a demotion, which goes through the
/// guarded `try_demote_user_unless_last_admin`), so this is its only direct
/// coverage.
pub async fn update_user_role_promotes_member_to_admin<S: AuthStore>(store: Arc<S>) {
    let member = seed_user(&*store, "users-store-update-role-promote", Role::Member).await;
    store
        .update_user_role(&member.id, Role::Admin)
        .await
        .unwrap();
    let reloaded = store.get_user(&member.id).await.unwrap().unwrap();
    assert_eq!(reloaded.role, Role::Admin);
}

/// The raw, unguarded `delete_user` — distinct from
/// `try_delete_user_unless_last_admin`: it has no last-admin protection at
/// all, so it is never used to delete an admin through `AuthService`.
pub async fn delete_user_raw_removes_the_user<S: AuthStore>(store: Arc<S>) {
    let member = seed_user(&*store, "users-store-delete-raw", Role::Member).await;
    assert!(store.delete_user(&member.id).await.unwrap());
    assert!(store.get_user(&member.id).await.unwrap().is_none());
}

pub async fn delete_user_raw_unknown_id_is_false<S: AuthStore>(store: Arc<S>) {
    assert!(!store
        .delete_user("users-store-delete-raw-unknown")
        .await
        .unwrap());
}

pub async fn count_users_reflects_every_insert<S: AuthStore>(store: Arc<S>) {
    assert_eq!(store.count_users().await.unwrap(), 0);
    seed_user(&*store, "users-store-count-1", Role::Member).await;
    assert_eq!(store.count_users().await.unwrap(), 1);
    seed_user(&*store, "users-store-count-2", Role::Admin).await;
    assert_eq!(store.count_users().await.unwrap(), 2);
}

/// `admin_exists` must key off `role`, not mere presence — a store full of
/// members alone must report `false`.
pub async fn admin_exists_is_false_with_only_members<S: AuthStore>(store: Arc<S>) {
    seed_user(&*store, "users-store-admin-exists-member", Role::Member).await;
    assert!(!store.admin_exists().await.unwrap());
}

/// Deterministic concurrency check: two admins racing to demote EACH OTHER
/// (and themselves) must never both succeed — that would leave zero
/// admins. Exactly one demotion must win; the other must observe
/// `LastAdmin` once only one admin remains.
pub async fn concurrent_demote_of_two_last_admins_never_reaches_zero<S: AuthStore>(store: Arc<S>) {
    let a1 = seed_user(&*store, "users-store-concurrent-demote-a1", Role::Admin).await;
    let a2 = seed_user(&*store, "users-store-concurrent-demote-a2", Role::Admin).await;

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let store1 = store.clone();
    let id1 = a1.id.clone();
    let barrier1 = barrier.clone();
    let task1 = tokio::spawn(async move {
        barrier1.wait().await;
        store1.try_demote_user_unless_last_admin(&id1).await
    });
    let store2 = store.clone();
    let id2 = a2.id.clone();
    let barrier2 = barrier.clone();
    let task2 = tokio::spawn(async move {
        barrier2.wait().await;
        store2.try_demote_user_unless_last_admin(&id2).await
    });

    let outcome1 = task1.await.unwrap().unwrap();
    let outcome2 = task2.await.unwrap().unwrap();
    let dones = [outcome1, outcome2]
        .into_iter()
        .filter(|o| *o == DemoteOutcome::Done)
        .count();
    assert_eq!(
        dones, 1,
        "exactly one of the two racing demotions must succeed"
    );

    let admins_remaining = store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .filter(|u| u.role == Role::Admin)
        .count();
    assert_eq!(
        admins_remaining, 1,
        "zero admins must never result from the race"
    );
}
