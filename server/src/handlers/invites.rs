//! Invite management (T6, specs/05-surfaces.md §3.1, §2's `invite`
//! CLI counterpart in `cli/src/cmds/invite.rs`).
//!
//! Admin-only management routes (`GET/POST /v1/invites`, `DELETE
//! /v1/invites/{id}`, `GET /v1/invites/requests`, `POST
//! /v1/invites/requests/{id}/approve|deny`) live in the first half of this
//! file, mounted on the `protected` router in `daemon::build_router`. The
//! public redeem/poll routes (`redeem_invite`, `poll_request` — `POST
//! /v1/invites/redeem`, `GET /v1/invites/requests/{id}`) are in the second
//! half, deliberately public — they *are* the join flow, mirroring how
//! `/authorize`/`/token` are public for the OAuth2 flow (T4) — and are
//! mounted on the unlayered `public` router instead.
//!
//! HTTP shape only (specs/01-architecture.md §1): the invite/access-request
//! state machine itself lives in `core::auth::AuthService`
//! (`create_invite`/`redeem_invite`/`approve_request`/`deny_request`/`poll_request`);
//! this module parses/renders HTTP and maps `core::Error` onto the shared
//! error taxonomy.

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use localdb_core::auth::{
    AuthStore as _, InviteMode, InviteRow, PollOutcome, Principal, RedeemOutcome, Role, UserRow,
};
use localdb_core::types::StoreVisibility;
use localdb_core::Error as CoreError;

use super::require_principal;
use crate::auth::base_url::resolve_base_url;
use crate::error::ApiError;
use crate::state::AppState;

/// A minimal user view for invite-flow responses — deliberately not
/// `handlers::users::UserView` (kept module-local rather than reaching into
/// a sibling handler module, matching this codebase's existing convention of
/// no cross-handler-module coupling).
#[derive(Debug, Serialize)]
pub struct InviteUserView {
    pub id: String,
    pub name: String,
    pub role: &'static str,
}

impl From<UserRow> for InviteUserView {
    fn from(u: UserRow) -> Self {
        InviteUserView {
            id: u.id,
            name: u.name,
            role: match u.role {
                Role::Admin => "admin",
                Role::Member => "member",
            },
        }
    }
}

fn invite_mode_to_str(mode: InviteMode) -> &'static str {
    match mode {
        InviteMode::Open => "open",
        InviteMode::Closed => "closed",
    }
}

fn parse_invite_mode(s: &str) -> Result<InviteMode, ApiError> {
    match s {
        "open" => Ok(InviteMode::Open),
        "closed" => Ok(InviteMode::Closed),
        other => Err(ApiError(CoreError::InvalidRequest {
            message: format!("unknown invite mode '{other}'; expected 'open' or 'closed'"),
        })),
    }
}

/// An invite as returned by admin-facing routes — never the plaintext token
/// (shown exactly once, at creation, in `CreateInviteResponse`).
#[derive(Debug, Serialize)]
pub struct InviteView {
    pub id: String,
    pub mode: &'static str,
    pub store_grants: Vec<String>,
    pub max_uses: u32,
    pub uses: u32,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

impl From<InviteRow> for InviteView {
    fn from(row: InviteRow) -> Self {
        InviteView {
            id: row.id,
            mode: invite_mode_to_str(row.mode),
            store_grants: row.store_grants,
            max_uses: row.max_uses,
            uses: row.uses,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            created_by: row.created_by,
            created_at: row.created_at,
        }
    }
}

/// `GET /v1/invites`: admin-only, lists every invite (no secrets).
pub async fn list_invites(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
) -> Result<Json<Vec<InviteView>>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let invites = state.auth_store().list_invites().await?;
    Ok(Json(invites.into_iter().map(InviteView::from).collect()))
}

#[derive(Debug, Deserialize)]
pub struct CreateInviteRequest {
    pub mode: String,
    #[serde(default)]
    pub stores: Vec<String>,
    #[serde(default = "default_max_uses")]
    pub max_uses: u32,
    /// Absolute RFC 3339 expiry, if any — the CLI computes this from a
    /// human-readable duration (`--expires 7d`) client-side via
    /// `localdb_core::auth::rfc3339_from_now`, rather than the server
    /// parsing duration strings.
    pub expires_at: Option<String>,
}

fn default_max_uses() -> u32 {
    1
}

/// The show-once response to `POST /v1/invites`: `token` is the plaintext
/// invite secret, never persisted or retrievable again after this response
/// (D1) — and `consent_url` is a ready-made link to the OAuth2 consent page
/// (T4 seam) pre-filled with it, for the `open`-mode browser flow.
#[derive(Debug, Serialize)]
pub struct CreateInviteResponse {
    pub id: String,
    pub mode: &'static str,
    pub store_grants: Vec<String>,
    pub max_uses: u32,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub token: String,
    pub consent_url: String,
}

/// `POST /v1/invites`: admin-only. Resolves each named store's visibility
/// (404 `store_not_found` for an unknown name) and rejects any `private`
/// one via `AuthService::create_invite`'s own D7 check.
pub async fn create_invite(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    headers: HeaderMap,
    Json(req): Json<CreateInviteRequest>,
) -> Result<(StatusCode, Json<CreateInviteResponse>), ApiError> {
    let principal = require_principal(principal)?;
    principal.require_admin().map_err(ApiError)?;

    let mode = parse_invite_mode(&req.mode)?;

    // Resolve the consent URL's base *before* creating the invite (the same
    // way discovery/T7 does — `server::auth::base_url::resolve_base_url`):
    // prefer the operator-configured `server.public_url` (correct behind a
    // TLS-terminating reverse proxy), falling back to the request's own
    // sanitized `Host` header only when no `public_url` is configured.
    // Building this from a hard-coded `http://` + raw `Host` header (as
    // before) ignored `public_url` entirely and could downgrade/break the
    // show-once invite link (finding #9). Failing this check up front —
    // rather than after `create_invite` below — avoids minting a usable
    // invite the caller then can't learn the token for.
    let host_header = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let base = resolve_base_url(state.public_url(), host_header).ok_or_else(|| {
        ApiError(CoreError::InvalidRequest {
            message: "cannot determine this server's base URL: no server.public_url is \
                      configured and the request's Host header is missing or invalid"
                .to_string(),
        })
    })?;

    let mut store_grants = Vec::with_capacity(req.stores.len());
    for name in &req.stores {
        let store = state.get_store_by_name(name).await?;
        let visibility =
            StoreVisibility::parse(&store.visibility).unwrap_or(StoreVisibility::Private);
        store_grants.push((name.clone(), visibility));
    }

    let issued = state
        .auth()
        .create_invite(
            mode,
            &store_grants,
            req.max_uses,
            req.expires_at.clone(),
            &principal.user_id,
        )
        .await?;

    let consent_url = format!("{base}/authorize?invite={}", issued.secret);

    Ok((
        StatusCode::CREATED,
        Json(CreateInviteResponse {
            id: issued.row.id,
            mode: invite_mode_to_str(issued.row.mode),
            store_grants: issued.row.store_grants,
            max_uses: issued.row.max_uses,
            expires_at: issued.row.expires_at,
            created_at: issued.row.created_at,
            token: issued.secret,
            consent_url,
        }),
    ))
}

/// `DELETE /v1/invites/{id}`: admin-only, revokes an invite.
pub async fn revoke_invite(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let revoked = state.auth_store().revoke_invite(&id).await?;
    if !revoked {
        return Err(ApiError(CoreError::InvalidRequest {
            message: format!("invite '{id}' not found or already revoked"),
        }));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// An access request as returned by admin-facing routes — never the
/// requester's secret.
#[derive(Debug, Serialize)]
pub struct AccessRequestView {
    pub id: String,
    pub invite_id: String,
    pub requested_name: String,
    pub state: &'static str,
    pub resulting_user_id: Option<String>,
    pub created_at: String,
    pub decided_at: Option<String>,
}

fn access_request_state_str(s: localdb_core::auth::AccessRequestState) -> &'static str {
    use localdb_core::auth::AccessRequestState::*;
    match s {
        Pending => "pending",
        Approved => "approved",
        Denied => "denied",
    }
}

impl From<localdb_core::auth::AccessRequestRow> for AccessRequestView {
    fn from(row: localdb_core::auth::AccessRequestRow) -> Self {
        AccessRequestView {
            id: row.id,
            invite_id: row.invite_id,
            requested_name: row.requested_name,
            state: access_request_state_str(row.state),
            resulting_user_id: row.resulting_user_id,
            created_at: row.created_at,
            decided_at: row.decided_at,
        }
    }
}

/// `GET /v1/invites/requests`: admin-only, lists every access request across
/// every invite (pending, approved, and denied alike — small admin-facing
/// surface, no pagination).
pub async fn list_access_requests(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
) -> Result<Json<Vec<AccessRequestView>>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let requests = state.auth_store().list_access_requests().await?;
    Ok(Json(
        requests.into_iter().map(AccessRequestView::from).collect(),
    ))
}

/// `POST /v1/invites/requests/{id}/approve`: admin-only. Creates the user
/// and applies the invite's store grants; no credential is minted here (see
/// `AuthService::approve_request`'s doc comment) — a fresh API key is minted
/// only when the requester's own next poll collects it
/// (`AuthService::poll_request`).
pub async fn approve_request(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
) -> Result<Json<InviteUserView>, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    let user = state.auth().approve_request(&id).await?;
    Ok(Json(InviteUserView::from(user)))
}

/// `POST /v1/invites/requests/{id}/deny`: admin-only.
pub async fn deny_request(
    State(state): State<AppState>,
    principal: Option<Extension<Principal>>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    require_principal(principal)?
        .require_admin()
        .map_err(ApiError)?;
    state.auth().deny_request(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Public routes (specs/05-surfaces.md §3.1): the invite-redemption/status
// surface, mounted outside the `require_auth` layer alongside the T4 OAuth
// routes — see `daemon::build_router`.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RedeemInviteRequest {
    pub token: String,
    pub name: String,
}

/// `POST /v1/invites/redeem` (public): `open` mode -> `201` with the new
/// user, its granted stores, and a show-once API key; `closed` mode -> `202`
/// with the pending request's id and secret to poll with.
pub async fn redeem_invite(
    State(state): State<AppState>,
    Json(req): Json<RedeemInviteRequest>,
) -> Result<Response, ApiError> {
    if req.token.trim().is_empty() || req.name.trim().is_empty() {
        return Err(ApiError(CoreError::InvalidRequest {
            message: "token and name are both required".to_string(),
        }));
    }
    match state.auth().redeem_invite(&req.token, &req.name).await? {
        RedeemOutcome::Open {
            user,
            grants,
            credential,
        } => Ok((
            StatusCode::CREATED,
            Json(serde_json::json!({
                "user": InviteUserView::from(user),
                "granted_stores": grants,
                "api_key": credential.secret,
            })),
        )
            .into_response()),
        RedeemOutcome::Closed {
            request_id,
            request_secret,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "request_id": request_id,
                "request_secret": request_secret,
                "poll": format!("/v1/invites/requests/{request_id}?secret={request_secret}"),
            })),
        )
            .into_response()),
    }
}

#[derive(Debug, Deserialize)]
pub struct PollRequestQuery {
    pub secret: Option<String>,
}

/// `GET /v1/invites/requests/{id}?secret=<request_secret>` (public): polls a
/// closed-mode access request's status. The secret is a query parameter
/// (documented choice, specs/05-surfaces.md §3.1) rather than a header — it
/// keeps the endpoint trivially `curl`-able and matches the `poll` hint
/// `POST /v1/invites/redeem` returns. An unknown `id` and a wrong `secret`
/// against a real one are deliberately indistinguishable (`Unauthorized`,
/// same message) — no existence oracle.
pub async fn poll_request(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<PollRequestQuery>,
) -> Result<Response, ApiError> {
    let secret = query.secret.unwrap_or_default();
    if secret.is_empty() {
        return Err(ApiError(CoreError::Unauthorized {
            message: "invalid access request id or secret".to_string(),
        }));
    }
    let outcome = state.auth().poll_request(&id, &secret).await?;
    let body = match outcome {
        PollOutcome::Pending => serde_json::json!({ "state": "pending" }),
        PollOutcome::Denied => serde_json::json!({ "state": "denied" }),
        PollOutcome::Approved { credential } => serde_json::json!({
            "state": "approved",
            "api_key": credential,
        }),
        PollOutcome::AlreadyCollected => serde_json::json!({ "state": "collected" }),
    };
    Ok((StatusCode::OK, Json(body)).into_response())
}
