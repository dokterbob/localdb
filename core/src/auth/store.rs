//! `AuthStore`: the persistence seam for all auth policy (D5).
//!
//! Mirrors `core::store::RetrievalStore`'s conventions: an object-safe async
//! trait, `Send + Sync + 'static`, with narrowly-scoped CRUD methods. The
//! concrete implementation lives in `store-libsql`; `core` itself does no
//! I/O.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::principal::Role;
use crate::Error;

/// A persisted user account. No passwords (D1) — identity is proven solely
/// by bearer secrets (`AuthTokenRow`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserRow {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub created_at: String,
}

/// Which kind of bearer secret an `AuthTokenRow` represents.
///
/// All three share one table (D1) — an API key is simply a token with
/// `kind = ApiKey`, no default expiry, and `last_used_at` tracked instead of
/// TTL enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Access,
    Refresh,
    ApiKey,
}

/// A persisted bearer token/API key.
///
/// Only `secret_hash` (blake3) is ever stored — the plaintext secret is
/// shown once at mint time and never persisted (D1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokenRow {
    pub id: String,
    pub user_id: String,
    pub kind: TokenKind,
    pub secret_hash: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    pub created_at: String,
    /// Refresh-token rotation family. Every refresh token minted from the
    /// same original login shares a `family_id`; reuse of any revoked
    /// member revokes the whole family (D1).
    pub family_id: Option<String>,
    /// The token ID this one replaced via rotation, if any.
    pub rotated_from: Option<String>,
}

/// A persisted OAuth2 authorization code (RFC 6749 §4.1, T4).
///
/// Single-use, bound at issue time to `client_id` + `redirect_uri` +
/// `code_challenge` (PKCE S256) so a code minted for one exchange can't be
/// replayed against a different client/redirect/verifier combination — see
/// `AuthService::redeem_auth_code`. Only `code_hash` (blake3) is ever
/// stored; the plaintext code is shown once, in the `Location` redirect from
/// `POST /authorize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCodeRow {
    pub id: String,
    pub client_id: String,
    pub user_id: String,
    pub code_hash: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub redirect_uri: String,
    pub expires_at: String,
    pub consumed_at: Option<String>,
    pub created_at: String,
}

/// A dynamically registered OAuth2 client (RFC 7591, T7).
///
/// Public clients only (D1's "no passwords" ethos extends here: no
/// `client_secret` is ever minted or stored) — `token_endpoint_auth_method`
/// is always `"none"` and is not persisted, since it never varies.
/// `redirect_uris` are matched **exactly** at `/authorize` time (T7 decision,
/// specs/05-surfaces.md §3.1): registered clients get no loopback-any-port
/// exception the way the built-in `localdb-cli` client does — see
/// `core::auth::client::validate_registration_redirect_uri`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthClientRow {
    pub id: String,
    pub client_name: Option<String>,
    /// The exact set of redirect URIs this client registered. Stored as a
    /// JSON array in the `oauth_clients.redirect_uris` column.
    pub redirect_uris: Vec<String>,
    pub created_at: String,
}

/// A store-name/user-id grant (D7). Normalized: one row per grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreGrantRow {
    pub store_name: String,
    pub user_id: String,
    pub granted_by: String,
    pub created_at: String,
}

/// Whether redeeming an invite requires admin approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InviteMode {
    /// Redemption immediately creates a user — no approval step.
    Open,
    /// Redemption creates a pending `AccessRequestRow`; an admin must approve.
    Closed,
}

/// A persisted invite.
///
/// The full redeem/approve state machine lands in T6; the table ships now
/// (D13) so a later ticket doesn't need another migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRow {
    pub id: String,
    pub token_hash: String,
    pub mode: InviteMode,
    /// Store names granted to the resulting user on redemption.
    pub store_grants: Vec<String>,
    pub max_uses: u32,
    pub uses: u32,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

/// State of a pending access request against a `closed`-mode invite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessRequestState {
    Pending,
    Approved,
    Denied,
}

/// A request to redeem a `closed`-mode invite, awaiting admin approval.
///
/// `secret_hash` (T6): the blake3 hash of the request secret minted at
/// redemption time (`AuthService::redeem_invite`) and shown once to the
/// requester then. On approval (`AuthService::approve_request`) that same
/// secret is promoted to the new user's live API key — this is deliberate:
/// it avoids ever holding a *second* plaintext credential in memory between
/// approval and the requester's next poll (see `AuthService::approve_request`
/// doc comment). `collected_at` (T6) guards the "handed out exactly once"
/// contract: `AuthStore::mark_access_request_collected` is the atomic
/// consume-once gate `poll_request` uses, mirroring `consume_auth_code`'s
/// convention for the OAuth2 authorization code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRequestRow {
    pub id: String,
    pub invite_id: String,
    pub requested_name: String,
    pub secret_hash: String,
    pub state: AccessRequestState,
    pub resulting_user_id: Option<String>,
    pub created_at: String,
    pub decided_at: Option<String>,
    pub collected_at: Option<String>,
}

/// Persistence seam for all auth policy state.
///
/// Implemented over libsql in `store-libsql`; `core` itself does no I/O
/// (D5). Object-safe so it can be boxed/`Arc`-shared across async tasks,
/// matching `RetrievalStore`'s conventions.
#[async_trait]
pub trait AuthStore: Send + Sync + 'static {
    // ------------------------------------------------------------------
    // Users
    // ------------------------------------------------------------------
    async fn create_user(&self, user: &UserRow) -> Result<(), Error>;
    async fn get_user(&self, id: &str) -> Result<Option<UserRow>, Error>;
    async fn get_user_by_name(&self, name: &str) -> Result<Option<UserRow>, Error>;
    async fn list_users(&self) -> Result<Vec<UserRow>, Error>;
    async fn update_user_role(&self, id: &str, role: Role) -> Result<(), Error>;
    async fn delete_user(&self, id: &str) -> Result<bool, Error>;
    /// Total user count — used by `AuthService`/callers that need a plain
    /// user census (specs/05-surfaces.md §3.1).
    async fn count_users(&self) -> Result<u64, Error>;
    /// Whether at least one `Role::Admin` user exists.
    ///
    /// Finding #5: the one-time setup-code bootstrap
    /// (`server::auth::generate_setup_code_if_needed`) must key off this, not
    /// `count_users() > 0` — a first user created without `--admin` (e.g.
    /// direct `localdb user add bob`) is a `Role::Member`, and suppressing
    /// the setup code on "any user exists" would leave the instance
    /// auth-enforced with zero admins and no way to create one via the API
    /// (every admin-management route requires an admin principal already).
    async fn admin_exists(&self) -> Result<bool, Error>;
    /// Pending first-admin setup; absence never implies an established admin needs recovery.
    async fn pending_bootstrap_user(&self) -> Result<Option<UserRow>, Error>;
    /// Atomically reuse the pending admin or create the first admin and its recovery marker.
    /// Reject when an established admin already exists.
    async fn begin_bootstrap(&self, user: &UserRow) -> Result<UserRow, Error>;
    async fn complete_bootstrap(&self, user_id: &str) -> Result<(), Error>;
    /// Atomically delete `id` unless it is currently the *sole* admin (D7's
    /// last-admin lockout guard): a single conditional `DELETE ... WHERE id
    /// = ? AND (role <> 'admin' OR (admin count) > 1)`, so two concurrent
    /// guarded deletes/demotes racing the same two-admin instance can never
    /// both succeed and leave zero admins. Returns `true` iff this call
    /// deleted the row; `false` covers both "unknown id" and "would have
    /// left zero admins" — `AuthService::delete_user` disambiguates the two
    /// for its error message via `is_last_admin`.
    async fn try_delete_user_unless_last_admin(&self, id: &str) -> Result<bool, Error>;
    /// Atomically demote `id` to `Role::Member` unless it is currently the
    /// sole admin — the demotion counterpart to
    /// `try_delete_user_unless_last_admin`, same conditional-UPDATE
    /// convention. Returns `true` iff this call performed the demotion;
    /// `false` covers "unknown id", "already not an admin", and "would have
    /// left zero admins" — `AuthService::set_user_role` disambiguates via
    /// `is_last_admin` before falling back to the plain `update_user_role`
    /// for the idempotent non-admin cases.
    async fn try_demote_user_unless_last_admin(&self, id: &str) -> Result<bool, Error>;

    // ------------------------------------------------------------------
    // Tokens
    // ------------------------------------------------------------------
    async fn insert_token(&self, token: &AuthTokenRow) -> Result<(), Error>;
    /// Consume the old refresh token and insert both replacements atomically.
    /// False means the token is no longer eligible; errors leave all three rows unchanged.
    async fn rotate_tokens(
        &self,
        old_id: &str,
        access: &AuthTokenRow,
        refresh: &AuthTokenRow,
    ) -> Result<bool, Error>;
    async fn find_token_by_hash(&self, secret_hash: &str) -> Result<Option<AuthTokenRow>, Error>;
    /// Look up a token by its own ID (not its secret hash) — used by
    /// `DELETE /v1/keys/{id}` to resolve the owning user before checking
    /// "self or admin" (specs/05-surfaces.md §3.1), where the caller only
    /// has the token's ID, never its secret.
    async fn find_token(&self, id: &str) -> Result<Option<AuthTokenRow>, Error>;
    async fn revoke_token(&self, id: &str) -> Result<bool, Error>;
    /// Revoke every token sharing `family_id`. Called on rotated-refresh
    /// -token reuse detection (D1).
    async fn revoke_token_family(&self, family_id: &str) -> Result<u64, Error>;
    async fn mark_token_used(&self, id: &str, used_at: &str) -> Result<(), Error>;
    async fn list_tokens_for_user(&self, user_id: &str) -> Result<Vec<AuthTokenRow>, Error>;

    // ------------------------------------------------------------------
    // OAuth2 authorization codes (T4)
    // ------------------------------------------------------------------
    async fn create_auth_code(&self, code: &AuthCodeRow) -> Result<(), Error>;
    async fn find_auth_code_by_hash(&self, code_hash: &str) -> Result<Option<AuthCodeRow>, Error>;
    /// Mark the code consumed iff it is not already consumed. Returns
    /// `true` if this call consumed it, `false` if it was already consumed
    /// (or unknown) — this is the atomic "consume-once" guard against a
    /// concurrent-redemption race (see `AuthService::redeem_auth_code`).
    async fn consume_auth_code(&self, id: &str, consumed_at: &str) -> Result<bool, Error>;

    // ------------------------------------------------------------------
    // OAuth2 dynamic client registration (RFC 7591, T7)
    // ------------------------------------------------------------------
    async fn create_oauth_client(&self, client: &OAuthClientRow) -> Result<(), Error>;
    async fn find_oauth_client(&self, id: &str) -> Result<Option<OAuthClientRow>, Error>;

    // ------------------------------------------------------------------
    // Grants
    // ------------------------------------------------------------------
    async fn grant_store(&self, grant: &StoreGrantRow) -> Result<(), Error>;
    async fn revoke_store_grant(&self, store_name: &str, user_id: &str) -> Result<bool, Error>;
    async fn list_grants_for_user(&self, user_id: &str) -> Result<Vec<StoreGrantRow>, Error>;
    async fn list_grants_for_store(&self, store_name: &str) -> Result<Vec<StoreGrantRow>, Error>;

    // ------------------------------------------------------------------
    // Invites + access requests
    // ------------------------------------------------------------------
    async fn create_invite(&self, invite: &InviteRow) -> Result<(), Error>;
    async fn find_invite_by_hash(&self, token_hash: &str) -> Result<Option<InviteRow>, Error>;
    async fn find_invite(&self, id: &str) -> Result<Option<InviteRow>, Error>;
    async fn list_invites(&self) -> Result<Vec<InviteRow>, Error>;
    async fn revoke_invite(&self, id: &str) -> Result<bool, Error>;
    /// Atomically reserve one use against `max_uses` (T6, D9): the
    /// conditional update `UPDATE invites SET uses = uses + 1 WHERE id = ?
    /// AND uses < max_uses AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now)`. Returns `true` iff this call reserved a slot
    /// (mirrors `consume_auth_code`'s "consume iff eligible" convention);
    /// `false` means the invite is no longer eligible.
    ///
    /// `AuthService::redeem_invite` calls this to RESERVE a use *before*
    /// attempting the mint (user-create / access-request-file) — the atomic
    /// gate that caps concurrent redemptions at `max_uses` even when they
    /// race under distinct requested names. If the mint then fails, the
    /// caller must call `release_invite_use` to give the reserved slot back
    /// so a failed redemption never permanently burns a use.
    async fn try_consume_invite_use(&self, id: &str) -> Result<bool, Error>;
    /// Release a use previously reserved by `try_consume_invite_use` when
    /// the subsequent mint failed. Restores `uses` by one (never below
    /// zero).
    async fn release_invite_use(&self, id: &str) -> Result<(), Error>;

    /// Create the user, apply grants, and approve a still-pending request in one transaction.
    async fn approve_access_request(
        &self,
        id: &str,
        user: &UserRow,
        grants: &[StoreGrantRow],
    ) -> Result<bool, Error>;
    async fn create_access_request(&self, req: &AccessRequestRow) -> Result<(), Error>;
    async fn find_access_request(&self, id: &str) -> Result<Option<AccessRequestRow>, Error>;
    async fn list_access_requests_for_invite(
        &self,
        invite_id: &str,
    ) -> Result<Vec<AccessRequestRow>, Error>;
    /// Every access request across every invite, newest-created last — backs
    /// `GET /v1/invites/requests` (T6). Small admin-facing surface (no
    /// pagination): the number of pending/decided join requests is expected
    /// to be tiny relative to, say, chunk counts.
    async fn list_access_requests(&self) -> Result<Vec<AccessRequestRow>, Error>;
    /// Atomically transition a pending access request to a terminal
    /// decision (`Approved` or `Denied`), iff it is currently `Pending`: a
    /// single conditional `UPDATE ... WHERE id = ? AND state = 'pending'`
    /// (same single-condition-in-the-WHERE-clause convention as
    /// `try_consume_invite_use`/`mark_access_request_collected`). Returns
    /// `true` iff this call performed the transition; `false` means the
    /// request was unknown or already decided (by a concurrent
    /// approve/deny), in which case the caller must not treat its own
    /// decision as having taken effect — see `AuthService::approve_request`
    /// and `AuthService::deny_request`.
    async fn try_decide_access_request(
        &self,
        id: &str,
        state: AccessRequestState,
        resulting_user_id: Option<&str>,
        decided_at: &str,
    ) -> Result<bool, Error>;
    /// Atomically mark an `Approved` access request's credential as
    /// collected, iff it hasn't been already (T6's "handed out exactly
    /// once" contract — mirrors `consume_auth_code`'s single-use guard).
    /// Returns `true` only if this call performed the transition; `false`
    /// if the request is unknown, not yet approved, or already collected —
    /// `AuthService::poll_request` treats every `false` outcome as
    /// `PollOutcome::AlreadyCollected` rather than distinguishing further,
    /// since by the time this is called the caller has already proven
    /// knowledge of the request secret.
    async fn mark_access_request_collected(
        &self,
        id: &str,
        collected_at: &str,
    ) -> Result<bool, Error>;
}

// ---------------------------------------------------------------------------
// FakeAuthStore — in-memory AuthStore for core unit tests.
// ---------------------------------------------------------------------------

/// An in-memory `AuthStore` for use in tests (mirrors `core::store::FakeStore`).
#[cfg(any(test, feature = "test-support"))]
mod fake;
#[cfg(any(test, feature = "test-support"))]
pub use fake::FakeAuthStore;

#[cfg(test)]
mod tests;
