//! Integration tests for `LibsqlAuthStore` against a real tmpdir libsql
//! database — every `AuthStore` mutation path is exercised here (coverage
//! gate: data-modifying paths >= 90%, specs/01-architecture.md §7).

use localdb_core::auth::{
    AccessRequestRow, AccessRequestState, AuthCodeRow, AuthStore, AuthTokenRow, InviteMode,
    InviteRow, Role, StoreGrantRow, TokenKind, UserRow,
};
use localdb_core::types::StoreVisibility;
use localdb_core::{Error, StoreBackend, StoreBackendConfig, StoreRow, VectorEncoding};
use tempfile::tempdir;

use super::LibsqlAuthStore;
use crate::SqliteBackend;

/// Build an `AuthStore` via the same public path a real caller uses
/// (`SqliteBackend::auth_store()`), exercising that accessor too rather than
/// reaching into `crate::connection::LibsqlDb` directly. Also returns the
/// backing `SqliteBackend` so grant tests can insert a real `stores` row —
/// `store_grants.store_name` has a `REFERENCES stores(name)` FK.
async fn make_store() -> (tempfile::TempDir, SqliteBackend, LibsqlAuthStore) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("localdb.db");
    let backend = SqliteBackend::open(StoreBackendConfig::local_path(
        path,
        4,
        VectorEncoding::Float32,
    ))
    .await
    .unwrap();
    let auth_store = backend.auth_store();
    (dir, backend, auth_store)
}

/// Insert a real `stores` row named `name` — required before granting
/// access to it (`store_grants.store_name` FK-references `stores(name)`).
async fn insert_store_row(backend: &SqliteBackend, name: &str) {
    backend
        .upsert_store(&StoreRow {
            id: format!("store-{name}"),
            name: name.to_string(),
            visibility: StoreVisibility::Shared,
            backend: "libsql".to_string(),
            indexing_policy: "{}".to_string(),
            policy_version: "v1".to_string(),
            created_at: "2026-06-10T12:00:00Z".to_string(),
        })
        .await
        .unwrap();
}

fn make_user(id: &str, name: &str, role: Role) -> UserRow {
    UserRow {
        id: id.to_string(),
        name: name.to_string(),
        role,
        created_at: "2026-06-10T12:00:00Z".to_string(),
    }
}

fn make_token(id: &str, user_id: &str, kind: TokenKind, secret_hash: &str) -> AuthTokenRow {
    AuthTokenRow {
        id: id.to_string(),
        user_id: user_id.to_string(),
        kind,
        secret_hash: secret_hash.to_string(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        family_id: None,
        rotated_from: None,
    }
}

mod auth_codes;
mod clients;
mod grants;
mod invites;
mod tokens;
mod users;

mod atomic;
