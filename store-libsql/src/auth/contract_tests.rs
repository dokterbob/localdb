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

use std::sync::Arc;

use localdb_core::auth::contract::SHARED_STORE_NAME;
use localdb_core::types::StoreVisibility;
use localdb_core::{StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};

use super::LibsqlAuthStore;
use crate::SqliteBackend;

async fn make_contract_store() -> Arc<LibsqlAuthStore> {
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
    // (`SqliteBackend::auth_store`), so dropping `backend` here is safe —
    // but the on-disk file must outlive every connection using it, so the
    // tempdir is intentionally never cleaned up by this test process.
    std::mem::forget(dir);
    auth_store
}

localdb_core::auth_store_contract_tests!(|| async { make_contract_store().await });
