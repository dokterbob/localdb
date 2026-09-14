//! `AuthService`: the policy layer over `AuthStore` (D5).
//!
//! Every method here is orchestration over the trait plus the pure crypto in
//! `token` and grant logic in `principal` — no direct I/O of its own.

mod invites;

use std::sync::Arc;

use crate::Error;
use crate::ids::new_ulid;
use crate::ingestion::now_rfc3339;
use crate::types::StoreVisibility;

use super::client;
use super::principal::{Principal, Role, StoreAccess};
use super::store::{
    AccessRequestRow, AccessRequestState, AuthCodeRow, AuthStore, AuthTokenRow, InviteMode,
    InviteRow, OAuthClientRow, StoreGrantRow, TokenKind, UserRow,
};
use super::token::{
    ACCESS_TOKEN_TTL_SECS, AUTH_CODE_TTL_SECS, REFRESH_TOKEN_TTL_SECS, hash_secret, is_expired,
    mint_secret, rfc3339_from_now, verify_pkce_s256, verify_secret,
};

/// A newly minted bearer token: the persisted row plus the plaintext secret
/// (shown to the caller exactly once — never persisted, never logged).
#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub row: AuthTokenRow,
    pub secret: String,
}

/// A newly created invite: the persisted row plus its plaintext secret
/// (shown once).
#[derive(Debug, Clone)]
pub struct IssuedInvite {
    pub row: InviteRow,
    pub secret: String,
}

/// A newly minted authorization code: the persisted row plus the plaintext
/// code (shown once, in the `POST /authorize` redirect's `code` param).
#[derive(Debug, Clone)]
pub struct IssuedAuthCode {
    pub row: AuthCodeRow,
    pub secret: String,
}

/// The auth policy layer. Generic over `AuthStore` so callers (server, cli)
/// can plug in the libsql-backed implementation; core tests use
/// `FakeAuthStore`.
pub struct AuthService<S: AuthStore> {
    store: Arc<S>,
}

impl<S: AuthStore> AuthService<S> {
    pub fn new(store: Arc<S>) -> Self {
        Self { store }
    }

    /// Create a new user. No passwords (D1) — callers mint a token
    /// (`issue_api_key` / `issue_access_token`) separately.
    pub async fn create_user(&self, name: &str, role: Role) -> Result<UserRow, Error> {
        if self.store.get_user_by_name(name).await?.is_some() {
            return Err(Error::InvalidRequest {
                message: format!("user '{name}' already exists"),
            });
        }
        let user = UserRow {
            id: new_ulid(),
            name: name.to_string(),
            role,
            created_at: now_rfc3339(),
        };
        self.store.create_user(&user).await?;
        Ok(user)
    }

    /// Mint a long-lived API key (D1): no default expiry; `last_used_at` is
    /// tracked on every successful `authenticate`.
    pub async fn issue_api_key(&self, user_id: &str) -> Result<IssuedToken, Error> {
        self.issue_token(user_id, TokenKind::ApiKey, None, None, None)
            .await
    }

    /// Mint a 1-hour access token (D1).
    pub async fn issue_access_token(&self, user_id: &str) -> Result<IssuedToken, Error> {
        self.issue_token(
            user_id,
            TokenKind::Access,
            Some(rfc3339_from_now(ACCESS_TOKEN_TTL_SECS)),
            None,
            None,
        )
        .await
    }

    /// Mint a 30-day refresh token (D1), starting a new rotation family.
    pub async fn issue_refresh_token(&self, user_id: &str) -> Result<IssuedToken, Error> {
        let family = new_ulid();
        self.issue_token(
            user_id,
            TokenKind::Refresh,
            Some(rfc3339_from_now(REFRESH_TOKEN_TTL_SECS)),
            Some(family),
            None,
        )
        .await
    }

    async fn issue_token(
        &self,
        user_id: &str,
        kind: TokenKind,
        expires_at: Option<String>,
        family_id: Option<String>,
        rotated_from: Option<String>,
    ) -> Result<IssuedToken, Error> {
        let minted = mint_secret();
        let row = AuthTokenRow {
            id: new_ulid(),
            user_id: user_id.to_string(),
            kind,
            secret_hash: minted.hash,
            expires_at,
            last_used_at: None,
            revoked_at: None,
            created_at: now_rfc3339(),
            family_id,
            rotated_from,
        };
        self.store.insert_token(&row).await?;
        Ok(IssuedToken {
            row,
            secret: minted.secret,
        })
    }

    /// Resolve a bearer secret to a `Principal`: validates the hash,
    /// expiry, and revocation state; updates `last_used_at` for API keys;
    /// and builds `StoreAccess` from the user's role + grants (D7).
    pub async fn authenticate(&self, bearer_secret: &str) -> Result<Principal, Error> {
        let hash = hash_secret(bearer_secret);
        let token =
            self.store
                .find_token_by_hash(&hash)
                .await?
                .ok_or_else(|| Error::Unauthorized {
                    message: "invalid bearer token".to_string(),
                })?;

        if token.revoked_at.is_some() {
            return Err(Error::Unauthorized {
                message: "token has been revoked".to_string(),
            });
        }
        if let Some(expires_at) = &token.expires_at {
            if is_expired(expires_at) {
                return Err(Error::Unauthorized {
                    message: "token has expired".to_string(),
                });
            }
        }

        if token.kind == TokenKind::Refresh {
            // A refresh secret is only ever meant to be exchanged via
            // `rotate_refresh_token`; accepting it as a bearer credential here
            // would let a 30-day refresh secret authenticate indefinitely
            // without ever rotating (defeats the rotation/reuse-detection
            // design). Access tokens and API keys remain valid bearer kinds.
            return Err(Error::Unauthorized {
                message: "refresh tokens cannot be used as bearer credentials".to_string(),
            });
        }

        let user =
            self.store
                .get_user(&token.user_id)
                .await?
                .ok_or_else(|| Error::Unauthorized {
                    message: "token's user no longer exists".to_string(),
                })?;

        if token.kind == TokenKind::ApiKey {
            self.store
                .mark_token_used(&token.id, &now_rfc3339())
                .await?;
        }

        let access = self.build_store_access(&user).await?;
        Ok(Principal {
            user_id: user.id,
            name: user.name,
            role: user.role,
            access,
        })
    }

    async fn build_store_access(&self, user: &UserRow) -> Result<StoreAccess, Error> {
        match user.role {
            Role::Admin => Ok(StoreAccess::All),
            Role::Member => {
                let grants = self.store.list_grants_for_user(&user.id).await?;
                Ok(StoreAccess::Granted(
                    grants.into_iter().map(|g| g.store_name).collect(),
                ))
            }
        }
    }

    /// Redeem a refresh token for a new access + refresh pair, rotating the
    /// refresh token (D1). Reuse of an already-rotated (revoked) refresh
    /// token revokes its entire family and returns `Unauthorized` — the
    /// standard mitigation for refresh-token theft.
    pub async fn rotate_refresh_token(
        &self,
        presented_secret: &str,
    ) -> Result<(IssuedToken, IssuedToken), Error> {
        let hash = hash_secret(presented_secret);
        let token =
            self.store
                .find_token_by_hash(&hash)
                .await?
                .ok_or_else(|| Error::Unauthorized {
                    message: "invalid refresh token".to_string(),
                })?;

        if token.kind != TokenKind::Refresh {
            return Err(Error::Unauthorized {
                message: "not a refresh token".to_string(),
            });
        }

        if token.revoked_at.is_some() {
            // Someone presented a refresh token that was already rotated
            // away (or explicitly revoked) — treat as theft and burn the
            // whole family so a stolen token can't be replayed indefinitely.
            if let Some(family) = &token.family_id {
                self.store.revoke_token_family(family).await?;
            }
            return Err(Error::Unauthorized {
                message: "refresh token reuse detected; session revoked".to_string(),
            });
        }

        if let Some(expires_at) = &token.expires_at {
            if is_expired(expires_at) {
                return Err(Error::Unauthorized {
                    message: "refresh token has expired".to_string(),
                });
            }
        }

        // `revoke_token` is an atomic "revoke iff not already revoked"
        // conditional update. If it returns `false`, we lost a race against
        // another concurrent rotation of this same refresh token — that
        // other caller already rotated it away, so minting a fresh pair
        // here too would produce two live refresh tokens in the same
        // family, defeating reuse detection. Treat this exactly like the
        // reuse-of-an-already-rotated-token branch above: burn the whole
        // family and fail closed.
        if !self.store.revoke_token(&token.id).await? {
            if let Some(family) = &token.family_id {
                self.store.revoke_token_family(family).await?;
            }
            return Err(Error::Unauthorized {
                message: "refresh token reuse detected; session revoked".to_string(),
            });
        }

        let family = token.family_id.clone().unwrap_or_else(new_ulid);
        let new_refresh = self
            .issue_token(
                &token.user_id,
                TokenKind::Refresh,
                Some(rfc3339_from_now(REFRESH_TOKEN_TTL_SECS)),
                Some(family),
                Some(token.id.clone()),
            )
            .await?;
        let new_access = self.issue_access_token(&token.user_id).await?;

        Ok((new_access, new_refresh))
    }

    /// Mint a single-use OAuth2 authorization code (RFC 6749 §4.1), bound to
    /// `client_id` + `redirect_uri` + PKCE `code_challenge` at issue time
    /// and expiring in [`AUTH_CODE_TTL_SECS`] (T4, specs/05-surfaces.md
    /// §3.1 R5). Redirect-uri validation policy is the caller's
    /// responsibility (`core::auth::validate_redirect_uri`, checked by
    /// `server/src/auth/oauth.rs` before calling this).
    pub async fn issue_auth_code(
        &self,
        client_id: &str,
        user_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
    ) -> Result<IssuedAuthCode, Error> {
        let minted = mint_secret();
        let row = AuthCodeRow {
            id: new_ulid(),
            client_id: client_id.to_string(),
            user_id: user_id.to_string(),
            code_hash: minted.hash,
            code_challenge: code_challenge.to_string(),
            code_challenge_method: "S256".to_string(),
            redirect_uri: redirect_uri.to_string(),
            expires_at: rfc3339_from_now(AUTH_CODE_TTL_SECS),
            consumed_at: None,
            created_at: now_rfc3339(),
        };
        self.store.create_auth_code(&row).await?;
        Ok(IssuedAuthCode {
            row,
            secret: minted.secret,
        })
    }

    /// Whether `client_id` is recognized: the built-in `localdb-cli` public
    /// client (pure, no store lookup — `client::is_known_client`) or a
    /// dynamically registered client (T7, `POST /register`) found in the
    /// `oauth_clients` table. Extends the T4 seam documented on
    /// `client::is_known_client`.
    pub async fn is_known_client(&self, client_id: &str) -> Result<bool, Error> {
        if client::is_known_client(client_id) {
            return Ok(true);
        }
        Ok(self.store.find_oauth_client(client_id).await?.is_some())
    }

    /// Validate `redirect_uri` for `client_id` (T7 extension of the T4 seam
    /// documented on `client::validate_redirect_uri`): the built-in
    /// `localdb-cli` client keeps its RFC 8252 §7.3 loopback-any-port
    /// exception; a registered client gets **exact match only** against its
    /// own stored `redirect_uris` — no loopback exception, since a registered
    /// client's redirect is a fixed, pre-declared value (specs/05-surfaces.md
    /// §3.1). An unknown `client_id` returns `Ok(false)`, matching
    /// `is_known_client`'s "unknown" case rather than an error.
    pub async fn validate_client_redirect_uri(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> Result<bool, Error> {
        if client::is_known_client(client_id) {
            return Ok(client::validate_redirect_uri(client_id, redirect_uri));
        }
        match self.store.find_oauth_client(client_id).await? {
            Some(row) => Ok(row.redirect_uris.iter().any(|u| u == redirect_uri)),
            None => Ok(false),
        }
    }

    /// Dynamic Client Registration (RFC 7591, T7): register a new public
    /// client with the given `redirect_uris` (validated one-by-one via
    /// `client::validate_registration_redirect_uri` — exact `https://` or
    /// loopback `http://` only, see that function's doc comment for the
    /// custom-scheme rejection rationale) and an optional display
    /// `client_name`. Mints a ULID `client_id`; there is no client secret
    /// (public clients only, mirroring `localdb-cli`'s own policy).
    pub async fn register_client(
        &self,
        redirect_uris: Vec<String>,
        client_name: Option<String>,
    ) -> Result<OAuthClientRow, Error> {
        if redirect_uris.is_empty() {
            return Err(Error::InvalidRequest {
                message: "redirect_uris is required and must not be empty".to_string(),
            });
        }
        for uri in &redirect_uris {
            if !client::validate_registration_redirect_uri(uri) {
                return Err(Error::InvalidRequest {
                    message: format!(
                        "redirect_uri '{uri}' is not allowed: must be an https:// URL or a \
                         loopback http://127.0.0.1[:port]/... or http://localhost[:port]/... URL"
                    ),
                });
            }
        }
        let row = OAuthClientRow {
            id: new_ulid(),
            client_name,
            redirect_uris,
            created_at: now_rfc3339(),
        };
        self.store.create_oauth_client(&row).await?;
        Ok(row)
    }

    /// Redeem an authorization code for the user it was issued to (RFC 6749
    /// §4.1.3 + RFC 7636 §4.6 PKCE verification).
    ///
    /// Checks, in order: the code is known, unconsumed, unexpired, and that
    /// `client_id` + `redirect_uri` exactly match what was bound at issue
    /// time, then verifies `code_verifier` against the stored S256
    /// challenge. On success the code is atomically marked consumed
    /// (`AuthStore::consume_auth_code` is a single "consume iff unconsumed"
    /// UPDATE, so a concurrent second redemption attempt always loses the
    /// race and fails here even if it passed every earlier check) and the
    /// associated user is returned.
    ///
    /// Every failure returns `Error::Unauthorized` with a distinct message;
    /// the HTTP surface (`server/src/auth/oauth.rs`) maps all of them
    /// uniformly to the RFC 6749 §5.2 `invalid_grant` JSON error — none of
    /// these messages are meant to leak which specific check failed to an
    /// untrusted caller.
    pub async fn redeem_auth_code(
        &self,
        code: &str,
        client_id: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<UserRow, Error> {
        let hash = hash_secret(code);
        let row = self
            .store
            .find_auth_code_by_hash(&hash)
            .await?
            .ok_or_else(|| Error::Unauthorized {
                message: "invalid or unknown authorization code".to_string(),
            })?;

        if row.consumed_at.is_some() {
            return Err(Error::Unauthorized {
                message: "authorization code already used".to_string(),
            });
        }
        if is_expired(&row.expires_at) {
            return Err(Error::Unauthorized {
                message: "authorization code expired".to_string(),
            });
        }
        if row.client_id != client_id {
            return Err(Error::Unauthorized {
                message: "client_id does not match the authorization code".to_string(),
            });
        }
        if row.redirect_uri != redirect_uri {
            return Err(Error::Unauthorized {
                message: "redirect_uri does not match the authorization code".to_string(),
            });
        }
        if !verify_pkce_s256(code_verifier, &row.code_challenge) {
            return Err(Error::Unauthorized {
                message: "PKCE verification failed".to_string(),
            });
        }

        let consumed = self
            .store
            .consume_auth_code(&row.id, &now_rfc3339())
            .await?;
        if !consumed {
            return Err(Error::Unauthorized {
                message: "authorization code already used".to_string(),
            });
        }

        self.store
            .get_user(&row.user_id)
            .await?
            .ok_or_else(|| Error::Unauthorized {
                message: "authorization code's user no longer exists".to_string(),
            })
    }

    /// RFC 7009 token revocation: revoke whatever bearer secret `secret`
    /// refers to (an access token, a refresh token — which revokes its
    /// whole rotation family, matching reuse-detection semantics — or an
    /// API key). Returns `true` if something was revoked, `false` if the
    /// secret was unknown or already revoked. Callers (`POST /revoke`)
    /// return HTTP 200 either way per RFC 7009 §2.2, which deliberately
    /// never leaks whether a presented token existed.
    pub async fn revoke_by_secret(&self, secret: &str) -> Result<bool, Error> {
        let hash = hash_secret(secret);
        let Some(token) = self.store.find_token_by_hash(&hash).await? else {
            return Ok(false);
        };
        if token.kind == TokenKind::Refresh {
            if let Some(family) = &token.family_id {
                let revoked = self.store.revoke_token_family(family).await?;
                return Ok(revoked > 0);
            }
        }
        self.store.revoke_token(&token.id).await
    }

    /// Grant a member access to a `shared`-visibility store (D7).
    ///
    /// Grants on `private` stores are rejected — `store_visibility` is
    /// passed in by the caller (server/cli), which looks it up via
    /// `StoreBackend`; `core` itself does not do I/O to fetch it.
    pub async fn grant_store(
        &self,
        store_name: &str,
        store_visibility: StoreVisibility,
        user_id: &str,
        granted_by: &str,
    ) -> Result<(), Error> {
        if store_visibility == StoreVisibility::Private {
            return Err(Error::Forbidden {
                message: format!(
                    "store '{store_name}' is private; private stores are admin-only \
                     and cannot be granted"
                ),
            });
        }
        let grant = StoreGrantRow {
            store_name: store_name.to_string(),
            user_id: user_id.to_string(),
            granted_by: granted_by.to_string(),
            created_at: now_rfc3339(),
        };
        self.store.grant_store(&grant).await
    }

    /// Revoke a previously granted store access. Returns `true` if a grant
    /// was removed, `false` if none existed.
    pub async fn revoke_store(&self, store_name: &str, user_id: &str) -> Result<bool, Error> {
        self.store.revoke_store_grant(store_name, user_id).await
    }

    /// `true` iff `user_id` names an admin and is the *only* remaining admin
    /// — the lockout condition guarded against by `delete_user` and
    /// `set_user_role` below. Unknown `user_id` is not "the last admin"
    /// (there is nothing to guard).
    async fn is_last_admin(&self, user_id: &str) -> Result<bool, Error> {
        let users = self.store.list_users().await?;
        let admin_count = users.iter().filter(|u| u.role == Role::Admin).count();
        let is_admin = users
            .iter()
            .any(|u| u.id == user_id && u.role == Role::Admin);
        Ok(is_admin && admin_count <= 1)
    }

    /// Delete a user, refusing if it would leave zero admins (D7 guard
    /// rail — avoids locking every admin out of the instance). Deleting a
    /// user cascades to their tokens and store grants at the schema level
    /// (`ON DELETE CASCADE`, specs/02-domain-model.md §9), so no separate
    /// token-revocation step is needed here.
    ///
    /// **Concurrency (finding #5 fix):** the guard is enforced atomically at
    /// the store layer (`AuthStore::try_delete_user_unless_last_admin`, a
    /// conditional `DELETE ... WHERE role <> 'admin' OR (admin count) >
    /// 1`), not merely by a check-then-act pre-check — two concurrent
    /// `delete_user`/`set_user_role` calls each observing 2 admins can no
    /// longer both proceed and leave zero admins, since the eligibility
    /// check is folded into the same statement as the mutation. If the
    /// guarded delete reports `false`, `is_last_admin` is consulted purely
    /// to build the right message: either `id` really is the sole admin
    /// (refuse), or the guard's `false` was simply "unknown id" / "not an
    /// admin at all" (report "not found" via `Ok(false)`, unchanged from
    /// before).
    pub async fn delete_user(&self, id: &str) -> Result<bool, Error> {
        if self.store.try_delete_user_unless_last_admin(id).await? {
            return Ok(true);
        }
        if self.is_last_admin(id).await? {
            return Err(Error::InvalidRequest {
                message: "cannot delete the last remaining admin account".to_string(),
            });
        }
        Ok(false)
    }

    /// Change a user's role, refusing to demote the last remaining admin to
    /// `member` (D7 guard rail — same lockout concern as `delete_user`).
    /// Promoting a member to admin is always allowed.
    ///
    /// **Concurrency (finding #5 fix):** demotion to `member` is enforced
    /// atomically at the store layer
    /// (`AuthStore::try_demote_user_unless_last_admin`), the same
    /// conditional-write pattern as `delete_user` above — see that method's
    /// doc comment for the race it closes. If the guarded demotion reports
    /// `false`, `is_last_admin` disambiguates a genuine last-admin refusal
    /// from "unknown id" / "already not an admin", falling back to the
    /// plain unconditional `update_user_role` for those idempotent cases
    /// (preserving both "not found" errors and a no-op member->member
    /// "demotion").
    pub async fn set_user_role(&self, id: &str, role: Role) -> Result<(), Error> {
        if role != Role::Member {
            return self.store.update_user_role(id, role).await;
        }
        if self.store.try_demote_user_unless_last_admin(id).await? {
            return Ok(());
        }
        if self.is_last_admin(id).await? {
            return Err(Error::InvalidRequest {
                message: "cannot demote the last remaining admin account".to_string(),
            });
        }
        self.store.update_user_role(id, role).await
    }

    /// D7 grant evaluation, delegated to the pure logic on `Principal`.
    pub fn can_read_store(
        &self,
        principal: &Principal,
        store_name: &str,
        visibility: StoreVisibility,
    ) -> bool {
        principal.can_read_store(store_name, visibility)
    }
}

/// Outcome of `AuthService::redeem_invite` (T6, D9).
#[derive(Debug, Clone)]
pub enum RedeemOutcome {
    /// `open`-mode invite: the user, its store grants (echoed back for
    /// display), and a show-once API-key credential, all created
    /// immediately.
    Open {
        user: UserRow,
        grants: Vec<String>,
        credential: Box<IssuedToken>,
    },
    /// `closed`-mode invite: a pending access request was filed. The
    /// requester polls `AuthService::poll_request` with `request_secret`
    /// until an admin decides.
    Closed {
        request_id: String,
        request_secret: String,
    },
}

/// Outcome of `AuthService::poll_request` (T6, D9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    Pending,
    /// The credential is handed back exactly once, on the poll that
    /// observes the `Approved` transition (see `poll_request`'s doc
    /// comment).
    Approved {
        credential: String,
    },
    Denied,
    /// The request was approved, but its credential was already collected
    /// by an earlier successful poll — terminal state.
    AlreadyCollected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::client::LOCALDB_CLI_CLIENT_ID;
    use crate::auth::store::FakeAuthStore;
    use crate::auth::token::TOKEN_PREFIX;

    fn service() -> AuthService<FakeAuthStore> {
        AuthService::new(Arc::new(FakeAuthStore::new()))
    }

    async fn make_open_invite(svc: &AuthService<FakeAuthStore>, max_uses: u32) -> IssuedInvite {
        svc.create_invite(
            InviteMode::Open,
            &[("docs".to_string(), StoreVisibility::Shared)],
            max_uses,
            None,
            "admin-1",
        )
        .await
        .unwrap()
    }

    async fn make_closed_invite(svc: &AuthService<FakeAuthStore>, max_uses: u32) -> IssuedInvite {
        svc.create_invite(
            InviteMode::Closed,
            &[("docs".to_string(), StoreVisibility::Shared)],
            max_uses,
            None,
            "admin-1",
        )
        .await
        .unwrap()
    }

    mod invites;
    mod tokens;
    mod users_and_grants;
}
