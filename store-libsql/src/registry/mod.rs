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

#[cfg(test)]
use crate::connection::LibsqlDb;

pub(crate) mod documents;
pub(crate) mod sources;
pub(crate) mod sql;
pub(crate) mod stores;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) async fn delete_sources_for_store(
    db: &LibsqlDb,
    store_id: &str,
) -> Result<u64, localdb_core::Error> {
    sources::delete_sources_for_store(db, store_id).await
}
