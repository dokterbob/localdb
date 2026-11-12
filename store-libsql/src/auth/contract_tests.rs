//! Instantiates the shared `AuthStore` contract suite
//! (`localdb_core::auth::contract`) against `LibsqlAuthStore`, backed by a
//! real tmpdir libsql database built the same public way
//! `store-libsql/src/auth/tests.rs`'s own `make_store` helper does
//! (`SqliteBackend::open` + `SqliteBackend::auth_store()`).
//!
//! `store_grants.store_name` has a `REFERENCES stores(name)` FK, so every
//! factory call also provisions one real `stores` row named
//! [`localdb_core::auth::contract::SHARED_STORE_NAME`] before handing the
//! store to the suite — the `FakeAuthStore` instantiation in `core` needs
//! no equivalent step, since the fake has no FK enforcement.
//!
//! The factory returns the store paired with its own `TempDir`: the
//! generated test destructures the pair, passes only the store into the
//! case function, and keeps the `TempDir` alive (as `_guard`) until the
//! test's own scope ends. Every case function consumes its `Arc<S>` by
//! value and drops the last reference before returning, so the store (and
//! the connection it holds) is always gone before the tempdir is removed —
//! no leaked directory per case, and no risk of deleting the backing file
//! out from under a still-open connection.

use std::sync::Arc;

use localdb_core::auth::contract::SHARED_STORE_NAME;
use localdb_core::types::StoreVisibility;
use localdb_core::{StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
use tempfile::TempDir;

use super::LibsqlAuthStore;
use crate::SqliteBackend;

async fn make_contract_store() -> (Arc<LibsqlAuthStore>, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        4,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    backend
        .upsert_store(&StoreRow {
            id: format!("store-{SHARED_STORE_NAME}"),
            name: SHARED_STORE_NAME.to_string(),
            visibility: StoreVisibility::Shared,
            backend: "libsql".to_string(),
            indexing_policy: "{}".to_string(),
            policy_version: "v1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();
    let auth_store = Arc::new(backend.auth_store());
    // `LibsqlAuthStore` holds its own clone of the underlying `Arc<LibsqlDb>`
    // (`SqliteBackend::auth_store`), so dropping `backend` here is safe.
    (auth_store, dir)
}

localdb_core::auth_store_contract_tests!(|| async { make_contract_store().await });
