//! Instantiates the shared `AuthStore` contract suite
//! (`crate::auth::contract`) against `FakeAuthStore`. `FakeAuthStore` has
//! no FK enforcement, so the factory needs no provisioning beyond a fresh,
//! empty store.

use std::sync::Arc;

use super::FakeAuthStore;

crate::auth_store_contract_tests!(|| async { Arc::new(FakeAuthStore::new()) });
