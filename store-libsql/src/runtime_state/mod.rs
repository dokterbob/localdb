//! Runtime registry CRUD over the unified schema.
//!
//! Replaces `localdb_core::config::runtime_state::RuntimeStateDb`. Operates
//! against `stores` and `sources` tables in the unified `LibsqlDb`, sharing
//! one connection with any number of `StoreHandle`s.
//!
//! Keyed by **`store_id`** (ULID), not `store_name`. CLI/server code resolves
//! name → id via `get_store_by_name` before reaching the per-store APIs.
//!
//! Foreign key cascade `stores → sources` is enforced by the schema (see
//! `unified_schema::create_sources`); `delete_sources_for_store` is a
//! convenience for partial cleanup.

use std::sync::Arc;

use crate::db::LibsqlDb;

mod rows;
mod sources;
mod sql;
mod stores;

#[cfg(test)]
mod tests;

pub use rows::{SourceRow, StoreRow};

pub struct RuntimeStateApi {
    db: Arc<LibsqlDb>,
}

impl RuntimeStateApi {
    pub fn new(db: Arc<LibsqlDb>) -> Self {
        Self { db }
    }
}
