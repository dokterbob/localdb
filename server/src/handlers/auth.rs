//! `GET /v1/auth/me` — the caller's resolved identity (specs/05-surfaces.md §3.1).
//!
//! Protected like every other `/v1` route: the `require_auth` middleware
//! resolves the bearer token to a `Principal` (or synthesizes
//! `Principal::local_trust()` in open mode) and inserts it into the request
//! extensions; this handler only reads it back. A missing extension means
//! the middleware did not run — fail closed with 401 rather than assuming
//! anything about the caller.

use axum::{Extension, Json};
use serde::Serialize;

use localdb_core::{
    auth::{Principal, Role, StoreAccess},
    Error as CoreError,
};

use crate::error::ApiError;

/// The caller's identity as returned by `GET /v1/auth/me`.
#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub user_id: String,
    pub name: String,
    /// `"admin"` or `"member"` (serde lowercase on `Role`).
    pub role: Role,
    pub store_access: StoreAccessView,
}

/// Serialized summary of a principal's store access: `"all"` for admins and
/// the local-trust principal, or the granted store-name list for members.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreAccessView {
    All,
    Granted { stores: Vec<String> },
}

impl From<Principal> for MeResponse {
    fn from(p: Principal) -> Self {
        let store_access = match p.access {
            StoreAccess::All => StoreAccessView::All,
            StoreAccess::Granted(set) => {
                let mut stores: Vec<String> = set.into_iter().collect();
                stores.sort();
                StoreAccessView::Granted { stores }
            }
        };
        MeResponse {
            user_id: p.user_id,
            name: p.name,
            role: p.role,
            store_access,
        }
    }
}

/// `GET /v1/auth/me`.
pub async fn get_me(principal: Option<Extension<Principal>>) -> Result<Json<MeResponse>, ApiError> {
    let Some(Extension(principal)) = principal else {
        // The auth middleware inserts a Principal on every request that
        // reaches a handler (local_trust in open mode). Its absence means
        // this route was served without the auth layer — fail closed.
        return Err(ApiError(CoreError::Unauthorized {
            message: "no authenticated principal on this request".to_string(),
        }));
    };
    Ok(Json(MeResponse::from(principal)))
}

#[cfg(test)]
mod tests;
