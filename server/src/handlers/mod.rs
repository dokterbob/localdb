//! Axum route handlers for the HTTP API.
//!
//! Every handler receives `State<AppState>` and returns a JSON response or
//! `ApiError`. The URL paths follow the resource list in specs/05-surfaces.md §3.
//!
//! Routes mounted at `/v1`:
//!   GET  /stores                  — list stores
//!   POST /stores                  — create runtime-owned store
//!   GET  /stores/:name            — get store by name
//!   PATCH /stores/:name           — update runtime-owned store
//!   DELETE /stores/:name          — delete runtime-owned store
//!   GET  /stores/:name/sources    — list sources for a store
//!   POST /stores/:name/sources    — add source to a store
//!   DELETE /sources/:id           — remove a source by ID
//!   GET  /documents/:id           — get document by ID
//!   POST /search                  — hybrid search
//!   POST /jobs                    — submit index job
//!   GET  /jobs/:id                — get job by ID
//!   GET  /status                  — daemon status
//!   GET  /config                  — resolved config (startup snapshot; no hot-reload)
//!   GET  /auth/me                 — the caller's authenticated principal

use axum::Extension;
use serde::{Deserialize, Serialize};

use localdb_core::{auth::Principal, Error as CoreError};

use crate::error::ApiError;

mod auth;
mod config;
mod discovery;
mod documents;
mod grants;
mod invites;
mod jobs;
mod keys;
mod search;
mod sources;
mod status;
mod stores;
mod users;

pub use auth::get_me;
pub use config::get_config;
pub use discovery::{oauth_authorization_server, oauth_protected_resource};
pub use documents::get_document;
pub use grants::{create_grant, delete_grant, list_grants};
pub use invites::{
    approve_request as approve_access_request, create_invite, deny_request as deny_access_request,
    list_access_requests, list_invites, poll_request as poll_access_request,
    redeem_invite as redeem_invite_public, revoke_invite,
};
pub use jobs::{create_job, get_job};
pub use keys::{create_key, list_keys, revoke_key};
pub use search::search;
pub use sources::{create_source, delete_source, list_sources};
pub use status::get_status;
pub use stores::{create_store, delete_store, get_store, list_stores, patch_store};
pub use users::{create_user, delete_user, list_users, patch_user};

#[cfg(test)]
mod tests;

/// Pull the `Principal` the `require_auth` middleware inserted out of the
/// request extensions, failing closed (`Unauthorized`) if it is absent —
/// mirrors `handlers::auth::get_me`'s existing convention. A missing
/// extension means this route was reached without the auth layer running
/// (e.g. a handler unit test that builds a bare router with no
/// `require_auth` layer); every real request path always carries one,
/// `Principal::local_trust()` included in open mode.
pub(crate) fn require_principal(
    principal: Option<Extension<Principal>>,
) -> Result<Principal, ApiError> {
    principal.map(|Extension(p)| p).ok_or_else(|| {
        ApiError(CoreError::Unauthorized {
            message: "no authenticated principal on this request".to_string(),
        })
    })
}

/// Cursor-based pagination parameters (from specs/05-surfaces.md §3).
#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

pub(crate) fn parse_cursor(cursor: Option<&str>) -> Result<usize, ApiError> {
    match cursor {
        None => Ok(0),
        Some(s) => s.parse::<usize>().map_err(|_| {
            ApiError(localdb_core::Error::InvalidRequest {
                message: format!(
                    "invalid pagination cursor '{s}'; expected a non-negative integer"
                ),
            })
        }),
    }
}

/// A paginated list response.
#[derive(Debug, Serialize)]
pub struct PaginatedList<T: Serialize> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub total: usize,
}

impl<T: Serialize> PaginatedList<T> {
    pub(crate) fn new(mut items: Vec<T>, offset: usize, limit: usize, total: usize) -> Self {
        let next_cursor = if offset + limit < total {
            Some(format!("{}", offset + limit))
        } else {
            None
        };
        items.truncate(limit);
        Self {
            items,
            next_cursor,
            total,
        }
    }
}
