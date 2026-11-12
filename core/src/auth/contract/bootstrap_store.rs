//! Direct `AuthStore::begin_bootstrap` edge cases: reusing a pending
//! bootstrap, refusing when an admin is already established, and picking
//! the first unused name suffix across several already-taken candidates.
//! `AuthService::begin_bootstrap` always passes the literal name `"admin"`,
//! so these drive the store directly to exercise the cases a fixed caller
//! never reaches on its own.

use std::sync::Arc;

use crate::auth::{AuthStore, Role, UserRow};
use crate::ids::new_ulid;
use crate::ingestion::now_rfc3339;

fn candidate(name: &str) -> UserRow {
    UserRow {
        id: new_ulid(),
        name: name.to_string(),
        role: Role::Admin,
        created_at: now_rfc3339(),
    }
}

/// A second `begin_bootstrap` call, before the first pending admin ever
/// completes, must reuse that same pending user rather than minting a
/// second one.
pub async fn begin_bootstrap_reuses_existing_pending_admin<S: AuthStore>(store: Arc<S>) {
    let first = store.begin_bootstrap(&candidate("admin")).await.unwrap();
    let second = store.begin_bootstrap(&candidate("admin")).await.unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(store.count_users().await.unwrap(), 1);
}

/// Once an admin is established (bootstrap completed, or an admin created
/// by any other path), `begin_bootstrap` must refuse rather than mint a
/// second pending admin.
pub async fn begin_bootstrap_refused_once_an_admin_exists<S: AuthStore>(store: Arc<S>) {
    let admin = candidate("already-an-admin");
    store.create_user(&admin).await.unwrap();

    let err = store
        .begin_bootstrap(&candidate("admin"))
        .await
        .unwrap_err();
    assert!(matches!(err, crate::Error::Unauthorized { .. }));
    assert_eq!(store.count_users().await.unwrap(), 1);
}

/// With `admin` and `admin-2` both already taken by members, the next
/// unused suffix is `admin-3`.
pub async fn begin_bootstrap_skips_several_occupied_suffixes<S: AuthStore>(store: Arc<S>) {
    let taken_1 = UserRow {
        role: Role::Member,
        ..candidate("admin")
    };
    let taken_2 = UserRow {
        role: Role::Member,
        ..candidate("admin-2")
    };
    store.create_user(&taken_1).await.unwrap();
    store.create_user(&taken_2).await.unwrap();

    let admin = store.begin_bootstrap(&candidate("admin")).await.unwrap();
    assert_eq!(admin.name, "admin-3");
}
