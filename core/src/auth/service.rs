//! `AuthService`: the policy layer over `AuthStore` (D5).
//!
//! Every method here is orchestration over the trait plus the pure crypto in
//! `token` and grant logic in `principal` — no direct I/O of its own.

use std::sync::Arc;

use crate::ids::new_ulid;
use crate::ingestion::now_rfc3339;
use crate::types::StoreVisibility;
use crate::Error;

use super::client;
use super::principal::{Principal, Role, StoreAccess};
use super::store::{
    AccessRequestRow, AccessRequestState, AuthCodeRow, AuthStore, AuthTokenRow, InviteMode,
    InviteRow, OAuthClientRow, StoreGrantRow, TokenKind, UserRow,
};
use super::token::{
    hash_secret, is_expired, mint_secret, rfc3339_from_now, verify_pkce_s256, verify_secret,
    ACCESS_TOKEN_TTL_SECS, AUTH_CODE_TTL_SECS, REFRESH_TOKEN_TTL_SECS,
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

        self.store.revoke_token(&token.id).await?;

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
    pub async fn delete_user(&self, id: &str) -> Result<bool, Error> {
        if self.is_last_admin(id).await? {
            return Err(Error::InvalidRequest {
                message: "cannot delete the last remaining admin account".to_string(),
            });
        }
        self.store.delete_user(id).await
    }

    /// Change a user's role, refusing to demote the last remaining admin to
    /// `member` (D7 guard rail — same lockout concern as `delete_user`).
    /// Promoting a member to admin is always allowed.
    pub async fn set_user_role(&self, id: &str, role: Role) -> Result<(), Error> {
        if role == Role::Member && self.is_last_admin(id).await? {
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

    /// Create an invite (T6): `mode` (open/closed), `store_grants` (name +
    /// visibility pairs — the caller resolves visibility via `StoreBackend`,
    /// the same seam `grant_store` uses), `max_uses` (>= 1), and an optional
    /// absolute RFC 3339 `expires_at`.
    ///
    /// Grants against a `private` store are rejected here at CREATE time
    /// (`Forbidden`, reusing D7's "private stores are admin-only and
    /// ungrantable" rule from `grant_store`) rather than deferred to
    /// redemption — a bad invite should fail loudly for the admin who typo'd
    /// a store name, not silently for whoever redeems it later.
    pub async fn create_invite(
        &self,
        mode: InviteMode,
        store_grants: &[(String, StoreVisibility)],
        max_uses: u32,
        expires_at: Option<String>,
        created_by: &str,
    ) -> Result<IssuedInvite, Error> {
        if max_uses == 0 {
            return Err(Error::InvalidRequest {
                message: "max_uses must be at least 1".to_string(),
            });
        }
        for (store_name, visibility) in store_grants {
            if *visibility == StoreVisibility::Private {
                return Err(Error::Forbidden {
                    message: format!(
                        "store '{store_name}' is private; only shared stores can be granted \
                         via invite"
                    ),
                });
            }
        }
        let minted = mint_secret();
        let row = InviteRow {
            id: new_ulid(),
            token_hash: minted.hash,
            mode,
            store_grants: store_grants.iter().map(|(name, _)| name.clone()).collect(),
            max_uses,
            uses: 0,
            expires_at,
            revoked_at: None,
            created_by: created_by.to_string(),
            created_at: now_rfc3339(),
        };
        self.store.create_invite(&row).await?;
        Ok(IssuedInvite {
            row,
            secret: minted.secret,
        })
    }

    /// Apply an invite's `store_grants` to a freshly created user, on behalf
    /// of the invite's own creator (`invite.created_by`) — shared by the
    /// `open`-mode immediate path (`redeem_invite`) and the `closed`-mode
    /// approval path (`approve_request`).
    async fn apply_invite_grants(&self, invite: &InviteRow, user_id: &str) -> Result<(), Error> {
        for store_name in &invite.store_grants {
            self.store
                .grant_store(&StoreGrantRow {
                    store_name: store_name.clone(),
                    user_id: user_id.to_string(),
                    granted_by: invite.created_by.clone(),
                    created_at: now_rfc3339(),
                })
                .await?;
        }
        Ok(())
    }

    /// Redeem an invite token (T6, D9): resolves the presented secret to an
    /// `InviteRow`, validates it, and either creates a user immediately
    /// (`open` mode) or files a pending `AccessRequestRow` (`closed` mode).
    ///
    /// Validation order — unknown/revoked/expired/exhausted all map to
    /// `Unauthorized` (mirroring `redeem_auth_code`'s "don't leak which
    /// check failed" convention; this is a public, unauthenticated route):
    /// 1. token resolves to a known invite,
    /// 2. not revoked,
    /// 3. not expired,
    /// 4. `uses < max_uses`.
    ///
    /// **Concurrency (documented choice):** `uses` is incremented *before*
    /// the user is created/access-request is filed. A burst of concurrent
    /// redemptions against a `max_uses = 1` invite can therefore over-count
    /// `uses` past `max_uses` (each racer passes the uses check before any
    /// of them increments) — a purely cosmetic overshoot visible to an admin
    /// via `invite list`/`GET /v1/invites`, and self-limiting since the
    /// invite is revoked/exhausted for everyone after. What must never
    /// happen is two racers both minting a user under the same
    /// `requested_name`: that hazard is independently closed by the
    /// `users.name` UNIQUE constraint enforced at the store level (see
    /// `store-libsql`'s `create_user`, which maps the UNIQUE violation to
    /// `Error::InvalidRequest`) — at worst one racer wins the name and the
    /// other gets an ordinary "already exists" error, never a double-mint.
    /// Incrementing `uses` first (rather than "check then act" with the
    /// increment last) is the fail-*safe* ordering here: the failure mode of
    /// over-counting uses is harmless, while under-counting (incrementing
    /// only after a successful mint) would let two racers both observe
    /// `uses < max_uses` and both attempt a mint against a `max_uses = 1`
    /// invite — same UNIQUE-constraint backstop either way, but
    /// over-counting is the more honest audit trail of "how many attempts
    /// actually raced here."
    pub async fn redeem_invite(
        &self,
        token_secret: &str,
        requested_name: &str,
    ) -> Result<RedeemOutcome, Error> {
        if requested_name.trim().is_empty() {
            return Err(Error::InvalidRequest {
                message: "requested name must not be empty".to_string(),
            });
        }
        let hash = hash_secret(token_secret);
        let invite = self
            .store
            .find_invite_by_hash(&hash)
            .await?
            .ok_or_else(|| Error::Unauthorized {
                message: "invalid or unknown invite token".to_string(),
            })?;

        if invite.revoked_at.is_some() {
            return Err(Error::Unauthorized {
                message: "invite has been revoked".to_string(),
            });
        }
        if let Some(expires_at) = &invite.expires_at {
            if is_expired(expires_at) {
                return Err(Error::Unauthorized {
                    message: "invite has expired".to_string(),
                });
            }
        }
        if invite.uses >= invite.max_uses {
            return Err(Error::Unauthorized {
                message: "invite has no remaining uses".to_string(),
            });
        }

        // See the doc comment above for why this happens before the mint.
        self.store.increment_invite_uses(&invite.id).await?;

        match invite.mode {
            InviteMode::Open => {
                let user = self.create_user(requested_name, Role::Member).await?;
                self.apply_invite_grants(&invite, &user.id).await?;
                let credential = self.issue_api_key(&user.id).await?;
                Ok(RedeemOutcome::Open {
                    user,
                    grants: invite.store_grants.clone(),
                    credential: Box::new(credential),
                })
            }
            InviteMode::Closed => {
                let minted = mint_secret();
                let request = AccessRequestRow {
                    id: new_ulid(),
                    invite_id: invite.id.clone(),
                    requested_name: requested_name.to_string(),
                    secret_hash: minted.hash,
                    state: AccessRequestState::Pending,
                    resulting_user_id: None,
                    created_at: now_rfc3339(),
                    decided_at: None,
                    collected_at: None,
                };
                self.store.create_access_request(&request).await?;
                Ok(RedeemOutcome::Closed {
                    request_id: request.id,
                    request_secret: minted.secret,
                })
            }
        }
    }

    /// Approve a pending closed-mode access request (T6): creates the user
    /// and applies the invite's store grants. No API-key token row is
    /// created here.
    ///
    /// The request secret (minted at `redeem_invite` time and known only to
    /// the requester) never becomes a credential — it exists purely to let
    /// the requester poll `poll_request` for their own request's status
    /// (device-authorization-grant pattern, RFC 8628). A fresh API key is
    /// minted in `poll_request`, at the moment the requester first collects
    /// it, so the durable credential is born only once someone has actually
    /// picked it up — never merely by an admin approving in the abstract.
    pub async fn approve_request(&self, request_id: &str) -> Result<UserRow, Error> {
        let request = self
            .store
            .find_access_request(request_id)
            .await?
            .ok_or_else(|| Error::InvalidRequest {
                message: format!("access request '{request_id}' not found"),
            })?;
        if request.state != AccessRequestState::Pending {
            return Err(Error::InvalidRequest {
                message: format!("access request '{request_id}' is no longer pending"),
            });
        }
        let invite = self
            .store
            .find_invite(&request.invite_id)
            .await?
            .ok_or(Error::Internal {
                message: format!(
                    "access request '{request_id}' references missing invite \
                     '{}'",
                    request.invite_id
                ),
                correlation_id: "approve_request_missing_invite".to_string(),
            })?;

        let user = self
            .create_user(&request.requested_name, Role::Member)
            .await?;
        self.apply_invite_grants(&invite, &user.id).await?;

        self.store
            .update_access_request_state(
                request_id,
                AccessRequestState::Approved,
                Some(&user.id),
                &now_rfc3339(),
            )
            .await?;

        Ok(user)
    }

    /// Deny a pending closed-mode access request (T6). No user is created;
    /// the requester's next `poll_request` observes `PollOutcome::Denied`.
    pub async fn deny_request(&self, request_id: &str) -> Result<(), Error> {
        let request = self
            .store
            .find_access_request(request_id)
            .await?
            .ok_or_else(|| Error::InvalidRequest {
                message: format!("access request '{request_id}' not found"),
            })?;
        if request.state != AccessRequestState::Pending {
            return Err(Error::InvalidRequest {
                message: format!("access request '{request_id}' is no longer pending"),
            });
        }
        self.store
            .update_access_request_state(
                request_id,
                AccessRequestState::Denied,
                None,
                &now_rfc3339(),
            )
            .await
    }

    /// Poll a closed-mode access request's status (T6, device-authorization
    /// -grant pattern, RFC 8628). `presented_secret` must match the
    /// request's own `secret_hash` — an unknown `request_id` and a wrong
    /// secret are deliberately indistinguishable (`Unauthorized`, same
    /// message), so a caller cannot use this endpoint to enumerate valid
    /// request IDs (no existence oracle; specs/05-surfaces.md §3.1).
    ///
    /// The request secret is poll-only — it is never promoted to a
    /// credential (it travels as a URL query parameter on every poll, which
    /// would otherwise leak into access logs/proxies/shell history as a
    /// long-lived, live-from-approval-time API key). Instead, on the
    /// transition into `Approved`, a *fresh* API key is minted for the
    /// request's resulting user and handed back exactly once:
    /// `AuthStore::mark_access_request_collected` is an atomic consume-once
    /// gate (mirroring `consume_auth_code`), so a second successful poll (or
    /// two concurrent ones racing the first) observes
    /// `PollOutcome::AlreadyCollected` instead of a credential — and no
    /// token row exists at all until the winning poll mints one.
    pub async fn poll_request(
        &self,
        request_id: &str,
        presented_secret: &str,
    ) -> Result<PollOutcome, Error> {
        let request = self
            .store
            .find_access_request(request_id)
            .await?
            .ok_or_else(|| Error::Unauthorized {
                message: "invalid access request id or secret".to_string(),
            })?;
        if !verify_secret(presented_secret, &request.secret_hash) {
            return Err(Error::Unauthorized {
                message: "invalid access request id or secret".to_string(),
            });
        }

        match request.state {
            AccessRequestState::Pending => Ok(PollOutcome::Pending),
            AccessRequestState::Denied => Ok(PollOutcome::Denied),
            AccessRequestState::Approved => {
                if request.collected_at.is_some() {
                    return Ok(PollOutcome::AlreadyCollected);
                }
                let collected = self
                    .store
                    .mark_access_request_collected(request_id, &now_rfc3339())
                    .await?;
                if collected {
                    let user_id =
                        request
                            .resulting_user_id
                            .as_deref()
                            .ok_or_else(|| Error::Internal {
                                message: format!(
                                    "access request '{request_id}' is approved but has no \
                                     resulting_user_id"
                                ),
                                correlation_id: "poll_request_missing_resulting_user".to_string(),
                            })?;
                    let issued = self.issue_api_key(user_id).await?;
                    Ok(PollOutcome::Approved {
                        credential: issued.secret,
                    })
                } else {
                    // Lost the race to a concurrent poll.
                    Ok(PollOutcome::AlreadyCollected)
                }
            }
        }
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

    #[tokio::test]
    async fn create_user_then_issue_api_key_authenticates() {
        let svc = service();
        let user = svc.create_user("alice", Role::Admin).await.unwrap();
        let issued = svc.issue_api_key(&user.id).await.unwrap();
        let principal = svc.authenticate(&issued.secret).await.unwrap();
        assert_eq!(principal.user_id, user.id);
        assert_eq!(principal.role, Role::Admin);
        assert_eq!(principal.access, StoreAccess::All);
    }

    #[tokio::test]
    async fn duplicate_user_name_rejected() {
        let svc = service();
        svc.create_user("alice", Role::Admin).await.unwrap();
        let err = svc.create_user("alice", Role::Member).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn wrong_secret_fails_authenticate() {
        let svc = service();
        let user = svc.create_user("bob", Role::Member).await.unwrap();
        svc.issue_api_key(&user.id).await.unwrap();
        let err = svc.authenticate("ldb_not-a-real-secret").await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn expired_token_rejected() {
        let svc = service();
        let user = svc.create_user("carol", Role::Member).await.unwrap();
        let minted = mint_secret();
        let row = AuthTokenRow {
            id: new_ulid(),
            user_id: user.id.clone(),
            kind: TokenKind::Access,
            secret_hash: minted.hash.clone(),
            expires_at: Some(rfc3339_from_now(-10)),
            last_used_at: None,
            revoked_at: None,
            created_at: now_rfc3339(),
            family_id: None,
            rotated_from: None,
        };
        svc.store.insert_token(&row).await.unwrap();
        let err = svc.authenticate(&minted.secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn revoked_token_rejected() {
        let svc = service();
        let user = svc.create_user("dave", Role::Member).await.unwrap();
        let issued = svc.issue_api_key(&user.id).await.unwrap();
        svc.store.revoke_token(&issued.row.id).await.unwrap();
        let err = svc.authenticate(&issued.secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn api_key_last_used_updated_on_authenticate() {
        let svc = service();
        let user = svc.create_user("erin", Role::Admin).await.unwrap();
        let issued = svc.issue_api_key(&user.id).await.unwrap();
        assert!(issued.row.last_used_at.is_none());
        svc.authenticate(&issued.secret).await.unwrap();
        let stored = svc
            .store
            .find_token_by_hash(&issued.row.secret_hash)
            .await
            .unwrap()
            .unwrap();
        assert!(stored.last_used_at.is_some());
    }

    #[tokio::test]
    async fn access_token_last_used_not_touched() {
        // Only API keys track last_used_at; access/refresh tokens are
        // short-lived and rotate/expire instead.
        let svc = service();
        let user = svc.create_user("felix", Role::Admin).await.unwrap();
        let issued = svc.issue_access_token(&user.id).await.unwrap();
        svc.authenticate(&issued.secret).await.unwrap();
        let stored = svc
            .store
            .find_token_by_hash(&issued.row.secret_hash)
            .await
            .unwrap()
            .unwrap();
        assert!(stored.last_used_at.is_none());
    }

    #[tokio::test]
    async fn refresh_rotation_happy_path() {
        let svc = service();
        let user = svc.create_user("frank", Role::Admin).await.unwrap();
        let refresh = svc.issue_refresh_token(&user.id).await.unwrap();

        let (new_access, new_refresh) = svc.rotate_refresh_token(&refresh.secret).await.unwrap();

        assert_eq!(new_refresh.row.family_id, refresh.row.family_id);
        assert_eq!(new_refresh.row.rotated_from, Some(refresh.row.id.clone()));

        // New access token authenticates.
        let principal = svc.authenticate(&new_access.secret).await.unwrap();
        assert_eq!(principal.user_id, user.id);

        // Old refresh token is now revoked (rotated away).
        let old = svc
            .store
            .find_token_by_hash(&refresh.row.secret_hash)
            .await
            .unwrap()
            .unwrap();
        assert!(old.revoked_at.is_some());
    }

    #[tokio::test]
    async fn reused_rotated_refresh_token_revokes_family_and_fails() {
        let svc = service();
        let user = svc.create_user("gina", Role::Admin).await.unwrap();
        let refresh = svc.issue_refresh_token(&user.id).await.unwrap();

        let (_new_access, new_refresh) = svc.rotate_refresh_token(&refresh.secret).await.unwrap();

        // Reuse the OLD (now-revoked) refresh secret — theft scenario.
        let err = svc.rotate_refresh_token(&refresh.secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));

        // The whole family — including the freshly rotated replacement —
        // must now be revoked.
        let rotated = svc
            .store
            .find_token_by_hash(&new_refresh.row.secret_hash)
            .await
            .unwrap()
            .unwrap();
        assert!(
            rotated.revoked_at.is_some(),
            "reuse must revoke the entire family"
        );

        // The now-revoked replacement can no longer authenticate either.
        let err = svc.authenticate(&new_refresh.secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn grant_policy_matrix() {
        let svc = service();
        let admin = Principal {
            user_id: "admin-1".into(),
            name: "admin".into(),
            role: Role::Admin,
            access: StoreAccess::All,
        };
        let member_with_grant = Principal {
            user_id: "m1".into(),
            name: "member1".into(),
            role: Role::Member,
            access: StoreAccess::Granted(["docs".to_string()].into_iter().collect()),
        };
        let member_no_grant = Principal {
            user_id: "m2".into(),
            name: "member2".into(),
            role: Role::Member,
            access: StoreAccess::Granted(Default::default()),
        };

        // Admin sees everything, private or shared.
        assert!(svc.can_read_store(&admin, "docs", StoreVisibility::Shared));
        assert!(svc.can_read_store(&admin, "secret", StoreVisibility::Private));

        // Member with a grant on a shared store: yes.
        assert!(svc.can_read_store(&member_with_grant, "docs", StoreVisibility::Shared));
        // Member without a grant: no.
        assert!(!svc.can_read_store(&member_no_grant, "docs", StoreVisibility::Shared));
        // Member with a grant attempting the same store as private: no —
        // private is admin-only regardless of any grant.
        assert!(!svc.can_read_store(&member_with_grant, "docs", StoreVisibility::Private));
    }

    #[tokio::test]
    async fn grant_store_rejects_private_visibility() {
        let svc = service();
        let err = svc
            .grant_store(
                "secret-store",
                StoreVisibility::Private,
                "user-1",
                "admin-1",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Forbidden { .. }));
    }

    #[tokio::test]
    async fn grant_store_allows_shared_visibility() {
        let svc = service();
        svc.grant_store("docs", StoreVisibility::Shared, "user-1", "admin-1")
            .await
            .unwrap();
        let grants = svc.store.list_grants_for_user("user-1").await.unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].store_name, "docs");
    }

    #[tokio::test]
    async fn revoke_store_removes_grant() {
        let svc = service();
        svc.grant_store("docs", StoreVisibility::Shared, "user-1", "admin-1")
            .await
            .unwrap();
        let removed = svc.revoke_store("docs", "user-1").await.unwrap();
        assert!(removed);
        let grants = svc.store.list_grants_for_user("user-1").await.unwrap();
        assert!(grants.is_empty());
    }

    #[tokio::test]
    async fn member_principal_reflects_grants_after_authenticate() {
        let svc = service();
        let user = svc.create_user("hank", Role::Member).await.unwrap();
        svc.grant_store("docs", StoreVisibility::Shared, &user.id, "admin-1")
            .await
            .unwrap();
        let issued = svc.issue_api_key(&user.id).await.unwrap();
        let principal = svc.authenticate(&issued.secret).await.unwrap();
        assert!(principal.can_read_store("docs", StoreVisibility::Shared));
        assert!(!principal.can_read_store("other", StoreVisibility::Shared));
    }

    // -----------------------------------------------------------------
    // OAuth2 authorization code (T4)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn issue_and_redeem_auth_code_happy_path() {
        let svc = service();
        let user = svc.create_user("ivy", Role::Admin).await.unwrap();
        let (verifier, challenge) = crate::auth::generate_pkce_pair();

        let issued = svc
            .issue_auth_code(
                "localdb-cli",
                &user.id,
                "http://127.0.0.1:1234/cb",
                &challenge,
            )
            .await
            .unwrap();

        let redeemed = svc
            .redeem_auth_code(
                &issued.secret,
                "localdb-cli",
                "http://127.0.0.1:1234/cb",
                &verifier,
            )
            .await
            .unwrap();
        assert_eq!(redeemed.id, user.id);
    }

    #[tokio::test]
    async fn redeem_auth_code_wrong_verifier_fails() {
        let svc = service();
        let user = svc.create_user("jack", Role::Admin).await.unwrap();
        let (_verifier, challenge) = crate::auth::generate_pkce_pair();
        let issued = svc
            .issue_auth_code("localdb-cli", &user.id, "http://127.0.0.1:1/cb", &challenge)
            .await
            .unwrap();

        let err = svc
            .redeem_auth_code(
                &issued.secret,
                "localdb-cli",
                "http://127.0.0.1:1/cb",
                "wrong-verifier",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_auth_code_is_single_use() {
        let svc = service();
        let user = svc.create_user("kim", Role::Admin).await.unwrap();
        let (verifier, challenge) = crate::auth::generate_pkce_pair();
        let issued = svc
            .issue_auth_code("localdb-cli", &user.id, "http://127.0.0.1:1/cb", &challenge)
            .await
            .unwrap();

        svc.redeem_auth_code(
            &issued.secret,
            "localdb-cli",
            "http://127.0.0.1:1/cb",
            &verifier,
        )
        .await
        .unwrap();

        let err = svc
            .redeem_auth_code(
                &issued.secret,
                "localdb-cli",
                "http://127.0.0.1:1/cb",
                &verifier,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_auth_code_expired_fails() {
        let svc = service();
        let user = svc.create_user("liam", Role::Admin).await.unwrap();
        let (verifier, challenge) = crate::auth::generate_pkce_pair();
        let minted = mint_secret();
        let row = AuthCodeRow {
            id: new_ulid(),
            client_id: "localdb-cli".to_string(),
            user_id: user.id.clone(),
            code_hash: minted.hash.clone(),
            code_challenge: challenge,
            code_challenge_method: "S256".to_string(),
            redirect_uri: "http://127.0.0.1:1/cb".to_string(),
            expires_at: rfc3339_from_now(-10),
            consumed_at: None,
            created_at: now_rfc3339(),
        };
        svc.store.create_auth_code(&row).await.unwrap();

        let err = svc
            .redeem_auth_code(
                &minted.secret,
                "localdb-cli",
                "http://127.0.0.1:1/cb",
                &verifier,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_auth_code_client_id_mismatch_fails() {
        let svc = service();
        let user = svc.create_user("mona", Role::Admin).await.unwrap();
        let (verifier, challenge) = crate::auth::generate_pkce_pair();
        let issued = svc
            .issue_auth_code("localdb-cli", &user.id, "http://127.0.0.1:1/cb", &challenge)
            .await
            .unwrap();

        let err = svc
            .redeem_auth_code(
                &issued.secret,
                "some-other-client",
                "http://127.0.0.1:1/cb",
                &verifier,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_auth_code_redirect_uri_mismatch_fails() {
        let svc = service();
        let user = svc.create_user("nora", Role::Admin).await.unwrap();
        let (verifier, challenge) = crate::auth::generate_pkce_pair();
        let issued = svc
            .issue_auth_code("localdb-cli", &user.id, "http://127.0.0.1:1/cb", &challenge)
            .await
            .unwrap();

        let err = svc
            .redeem_auth_code(
                &issued.secret,
                "localdb-cli",
                "http://127.0.0.1:9999/cb",
                &verifier,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_auth_code_unknown_code_fails() {
        let svc = service();
        let err = svc
            .redeem_auth_code(
                "ldb_not-a-real-code",
                "localdb-cli",
                "http://127.0.0.1:1/cb",
                "v",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    // -----------------------------------------------------------------
    // RFC 7009 revocation
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn revoke_by_secret_revokes_api_key() {
        let svc = service();
        let user = svc.create_user("oscar", Role::Admin).await.unwrap();
        let issued = svc.issue_api_key(&user.id).await.unwrap();

        assert!(svc.revoke_by_secret(&issued.secret).await.unwrap());
        let err = svc.authenticate(&issued.secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn revoke_by_secret_revokes_whole_refresh_family() {
        let svc = service();
        let user = svc.create_user("pia", Role::Admin).await.unwrap();
        let refresh = svc.issue_refresh_token(&user.id).await.unwrap();
        let (_new_access, new_refresh) = svc.rotate_refresh_token(&refresh.secret).await.unwrap();

        assert!(svc.revoke_by_secret(&new_refresh.secret).await.unwrap());

        let err = svc
            .rotate_refresh_token(&new_refresh.secret)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn revoke_by_secret_unknown_token_returns_false() {
        let svc = service();
        assert!(!svc.revoke_by_secret("ldb_unknown").await.unwrap());
    }

    #[tokio::test]
    async fn revoke_by_secret_already_revoked_returns_false() {
        let svc = service();
        let user = svc.create_user("quinn", Role::Admin).await.unwrap();
        let issued = svc.issue_api_key(&user.id).await.unwrap();
        assert!(svc.revoke_by_secret(&issued.secret).await.unwrap());
        assert!(!svc.revoke_by_secret(&issued.secret).await.unwrap());
    }

    // -----------------------------------------------------------------
    // T5: last-admin guard rails
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn delete_user_refuses_the_last_admin() {
        let svc = service();
        let admin = svc.create_user("only-admin", Role::Admin).await.unwrap();

        let err = svc.delete_user(&admin.id).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
        assert!(svc.store.get_user(&admin.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_user_allows_a_non_last_admin() {
        let svc = service();
        let admin1 = svc.create_user("admin1", Role::Admin).await.unwrap();
        let admin2 = svc.create_user("admin2", Role::Admin).await.unwrap();

        assert!(svc.delete_user(&admin1.id).await.unwrap());
        assert!(svc.store.get_user(&admin2.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_user_allows_deleting_a_member() {
        let svc = service();
        let admin = svc.create_user("solo-admin", Role::Admin).await.unwrap();
        let member = svc.create_user("some-member", Role::Member).await.unwrap();

        assert!(svc.delete_user(&member.id).await.unwrap());
        // The sole admin is untouched and still present.
        assert!(svc.store.get_user(&admin.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn set_user_role_refuses_to_demote_the_last_admin() {
        let svc = service();
        let admin = svc.create_user("only-admin2", Role::Admin).await.unwrap();

        let err = svc
            .set_user_role(&admin.id, Role::Member)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
        let reloaded = svc.store.get_user(&admin.id).await.unwrap().unwrap();
        assert_eq!(reloaded.role, Role::Admin, "role must be unchanged");
    }

    #[tokio::test]
    async fn set_user_role_allows_demoting_a_non_last_admin() {
        let svc = service();
        let admin1 = svc.create_user("admin1b", Role::Admin).await.unwrap();
        let admin2 = svc.create_user("admin2b", Role::Admin).await.unwrap();

        svc.set_user_role(&admin1.id, Role::Member).await.unwrap();
        let reloaded = svc.store.get_user(&admin1.id).await.unwrap().unwrap();
        assert_eq!(reloaded.role, Role::Member);
        // The remaining admin is unaffected.
        let admin2_reloaded = svc.store.get_user(&admin2.id).await.unwrap().unwrap();
        assert_eq!(admin2_reloaded.role, Role::Admin);
    }

    #[tokio::test]
    async fn set_user_role_allows_promoting_a_member_to_admin() {
        let svc = service();
        svc.create_user("solo-admin2", Role::Admin).await.unwrap();
        let member = svc.create_user("promotable", Role::Member).await.unwrap();

        svc.set_user_role(&member.id, Role::Admin).await.unwrap();
        let reloaded = svc.store.get_user(&member.id).await.unwrap().unwrap();
        assert_eq!(reloaded.role, Role::Admin);
    }

    #[tokio::test]
    async fn create_invite_mints_a_show_once_secret() {
        let svc = service();
        let issued = svc
            .create_invite(
                InviteMode::Open,
                &[("docs".to_string(), StoreVisibility::Shared)],
                1,
                None,
                "admin-1",
            )
            .await
            .unwrap();
        assert!(issued.secret.starts_with(TOKEN_PREFIX));
        assert_eq!(issued.row.uses, 0);
        assert_eq!(issued.row.max_uses, 1);
        assert_eq!(issued.row.store_grants, vec!["docs".to_string()]);
    }

    #[tokio::test]
    async fn create_invite_rejects_private_store_grant() {
        let svc = service();
        let err = svc
            .create_invite(
                InviteMode::Open,
                &[("secret-store".to_string(), StoreVisibility::Private)],
                1,
                None,
                "admin-1",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Forbidden { .. }));
    }

    #[tokio::test]
    async fn create_invite_rejects_zero_max_uses() {
        let svc = service();
        let err = svc
            .create_invite(InviteMode::Open, &[], 0, None, "admin-1")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    // -----------------------------------------------------------------
    // T6: invite redeem / approve / deny / poll state machine
    // -----------------------------------------------------------------

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

    #[tokio::test]
    async fn redeem_open_invite_happy_path_creates_user_grants_and_credential() {
        let svc = service();
        let issued = make_open_invite(&svc, 1).await;

        let outcome = svc.redeem_invite(&issued.secret, "newbie").await.unwrap();
        let RedeemOutcome::Open {
            user,
            grants,
            credential,
        } = outcome
        else {
            panic!("expected Open outcome");
        };
        assert_eq!(user.name, "newbie");
        assert_eq!(user.role, Role::Member);
        assert_eq!(grants, vec!["docs".to_string()]);
        assert!(credential.secret.starts_with(TOKEN_PREFIX));

        // The credential actually authenticates as the new user with the grant.
        let principal = svc.authenticate(&credential.secret).await.unwrap();
        assert_eq!(principal.user_id, user.id);
        assert!(principal.can_read_store("docs", StoreVisibility::Shared));

        let invite = svc
            .store
            .find_invite(&issued.row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(invite.uses, 1);
    }

    #[tokio::test]
    async fn redeem_open_invite_max_uses_one_double_redeem_fails() {
        let svc = service();
        let issued = make_open_invite(&svc, 1).await;

        svc.redeem_invite(&issued.secret, "first").await.unwrap();
        let err = svc
            .redeem_invite(&issued.secret, "second")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_open_invite_max_uses_two_allows_two_distinct_names() {
        let svc = service();
        let issued = make_open_invite(&svc, 2).await;

        svc.redeem_invite(&issued.secret, "first").await.unwrap();
        svc.redeem_invite(&issued.secret, "second").await.unwrap();
        let invite = svc
            .store
            .find_invite(&issued.row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(invite.uses, 2);
    }

    #[tokio::test]
    async fn redeem_invite_rejects_duplicate_requested_name() {
        let svc = service();
        svc.create_user("taken", Role::Member).await.unwrap();
        let issued = make_open_invite(&svc, 5).await;

        let err = svc
            .redeem_invite(&issued.secret, "taken")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn redeem_invite_unknown_token_fails() {
        let svc = service();
        let err = svc
            .redeem_invite("ldb_not-a-real-invite", "someone")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_invite_revoked_fails() {
        let svc = service();
        let issued = make_open_invite(&svc, 1).await;
        svc.store.revoke_invite(&issued.row.id).await.unwrap();

        let err = svc
            .redeem_invite(&issued.secret, "someone")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_invite_expired_fails() {
        let svc = service();
        let minted = mint_secret();
        let invite = InviteRow {
            id: new_ulid(),
            token_hash: minted.hash,
            mode: InviteMode::Open,
            store_grants: vec![],
            max_uses: 1,
            uses: 0,
            expires_at: Some(rfc3339_from_now(-10)),
            revoked_at: None,
            created_by: "admin-1".to_string(),
            created_at: now_rfc3339(),
        };
        svc.store.create_invite(&invite).await.unwrap();

        let err = svc
            .redeem_invite(&minted.secret, "someone")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn redeem_closed_invite_happy_path_files_pending_request() {
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;

        let outcome = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap();
        let RedeemOutcome::Closed {
            request_id,
            request_secret,
        } = outcome
        else {
            panic!("expected Closed outcome");
        };
        assert!(request_secret.starts_with(TOKEN_PREFIX));

        let poll = svc
            .poll_request(&request_id, &request_secret)
            .await
            .unwrap();
        assert_eq!(poll, PollOutcome::Pending);
    }

    #[tokio::test]
    async fn closed_invite_approve_then_poll_once_then_already_collected() {
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;
        let RedeemOutcome::Closed {
            request_id,
            request_secret,
        } = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap()
        else {
            panic!("expected Closed outcome");
        };

        let user = svc.approve_request(&request_id).await.unwrap();
        assert_eq!(user.name, "requester");
        assert_eq!(user.role, Role::Member);
        assert!(svc.store.get_user(&user.id).await.unwrap().is_some());

        // First poll after approval: a freshly minted credential is handed back.
        let first = svc
            .poll_request(&request_id, &request_secret)
            .await
            .unwrap();
        let PollOutcome::Approved { credential } = first else {
            panic!("expected Approved on first post-approval poll, got {first:?}");
        };
        assert_ne!(
            credential, request_secret,
            "the poll-only request secret must never become the live credential"
        );

        // The freshly minted credential actually authenticates as the newly
        // created user, with the invite's store grants applied.
        let principal = svc.authenticate(&credential).await.unwrap();
        assert_eq!(principal.user_id, user.id);
        assert!(principal.can_read_store("docs", StoreVisibility::Shared));

        // Second poll: terminal "already collected" state, not a credential again.
        let second = svc
            .poll_request(&request_id, &request_secret)
            .await
            .unwrap();
        assert_eq!(second, PollOutcome::AlreadyCollected);
    }

    #[tokio::test]
    async fn closed_invite_request_secret_never_authenticates() {
        // Pin: the request secret is poll-only, both before and after
        // collection — it must never itself work as a bearer credential,
        // since it travels as a URL query parameter on every poll (access
        // logs, proxies, shell history).
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;
        let RedeemOutcome::Closed {
            request_id,
            request_secret,
        } = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap()
        else {
            panic!("expected Closed outcome");
        };

        // Not a credential while pending.
        let err = svc.authenticate(&request_secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));

        svc.approve_request(&request_id).await.unwrap();

        // Not a credential immediately after approval either (live from
        // approval time was exactly the defect being fixed).
        let err = svc.authenticate(&request_secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));

        svc.poll_request(&request_id, &request_secret)
            .await
            .unwrap();

        // Still not a credential after the collecting poll.
        let err = svc.authenticate(&request_secret).await.unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn closed_invite_approve_without_poll_mints_no_api_key_token() {
        // Pin: the durable credential is born only at first successful
        // collection, never merely by approval — an approved-but-never
        // -polled request must leave no API-key token row for its user.
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;
        let RedeemOutcome::Closed { request_id, .. } = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap()
        else {
            panic!("expected Closed outcome");
        };

        let user = svc.approve_request(&request_id).await.unwrap();

        let tokens = svc.store.list_tokens_for_user(&user.id).await.unwrap();
        assert!(
            tokens.is_empty(),
            "approval alone must not mint any credential for the new user"
        );
    }

    #[tokio::test]
    async fn closed_invite_deny_then_poll_denied() {
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;
        let RedeemOutcome::Closed {
            request_id,
            request_secret,
        } = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap()
        else {
            panic!("expected Closed outcome");
        };

        svc.deny_request(&request_id).await.unwrap();
        let poll = svc
            .poll_request(&request_id, &request_secret)
            .await
            .unwrap();
        assert_eq!(poll, PollOutcome::Denied);

        // No user was created.
        assert!(svc
            .store
            .get_user_by_name("requester")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn approve_request_twice_fails() {
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;
        let RedeemOutcome::Closed { request_id, .. } = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap()
        else {
            panic!("expected Closed outcome");
        };

        svc.approve_request(&request_id).await.unwrap();
        let err = svc.approve_request(&request_id).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn deny_request_unknown_id_fails() {
        let svc = service();
        let err = svc.deny_request("nonexistent").await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn poll_request_wrong_secret_fails() {
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;
        let RedeemOutcome::Closed { request_id, .. } = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap()
        else {
            panic!("expected Closed outcome");
        };

        let err = svc
            .poll_request(&request_id, "ldb_wrong-secret")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized { .. }));
    }

    #[tokio::test]
    async fn poll_request_unknown_id_and_wrong_secret_are_indistinguishable() {
        // No existence oracle (specs/05-surfaces.md §3.1): both an unknown
        // request id and a wrong secret against a real one must produce the
        // exact same error shape.
        let svc = service();
        let issued = make_closed_invite(&svc, 1).await;
        let RedeemOutcome::Closed { request_id, .. } = svc
            .redeem_invite(&issued.secret, "requester")
            .await
            .unwrap()
        else {
            panic!("expected Closed outcome");
        };

        let unknown_id_err = svc
            .poll_request("totally-unknown-id", "ldb_whatever")
            .await
            .unwrap_err();
        let wrong_secret_err = svc
            .poll_request(&request_id, "ldb_wrong-secret")
            .await
            .unwrap_err();

        assert!(matches!(unknown_id_err, Error::Unauthorized { .. }));
        assert!(matches!(wrong_secret_err, Error::Unauthorized { .. }));
        assert_eq!(unknown_id_err.to_string(), wrong_secret_err.to_string());
    }

    #[tokio::test]
    async fn create_invite_with_grants_on_shared_store_then_redeem_grants_access() {
        let svc = service();
        let issued = svc
            .create_invite(
                InviteMode::Open,
                &[("docs".to_string(), StoreVisibility::Shared)],
                1,
                None,
                "admin-1",
            )
            .await
            .unwrap();
        let RedeemOutcome::Open { user, .. } =
            svc.redeem_invite(&issued.secret, "grantee").await.unwrap()
        else {
            panic!("expected Open outcome");
        };
        let grants = svc.store.list_grants_for_user(&user.id).await.unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].store_name, "docs");
    }

    // -----------------------------------------------------------------
    // T7: dynamic client registration + client resolution
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn is_known_client_recognizes_builtin_and_registered() {
        let svc = service();
        assert!(svc.is_known_client(LOCALDB_CLI_CLIENT_ID).await.unwrap());
        assert!(!svc.is_known_client("nonexistent").await.unwrap());

        let registered = svc
            .register_client(
                vec!["http://127.0.0.1:4000/cb".to_string()],
                Some("Test Client".to_string()),
            )
            .await
            .unwrap();
        assert!(svc.is_known_client(&registered.id).await.unwrap());
    }

    #[tokio::test]
    async fn register_client_rejects_empty_redirect_uris() {
        let svc = service();
        let err = svc.register_client(vec![], None).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn register_client_rejects_invalid_redirect_uri() {
        let svc = service();
        let err = svc
            .register_client(vec!["http://evil.com/cb".to_string()], None)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest { .. }));
    }

    #[tokio::test]
    async fn register_client_accepts_https_and_loopback() {
        let svc = service();
        let row = svc
            .register_client(
                vec![
                    "https://app.example.com/cb".to_string(),
                    "http://127.0.0.1:9999/cb".to_string(),
                ],
                Some("Multi Redirect Client".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(row.redirect_uris.len(), 2);
        assert!(!row.id.is_empty());
    }

    #[tokio::test]
    async fn validate_client_redirect_uri_registered_client_exact_match_only() {
        let svc = service();
        let row = svc
            .register_client(vec!["https://app.example.com/cb".to_string()], None)
            .await
            .unwrap();

        assert!(svc
            .validate_client_redirect_uri(&row.id, "https://app.example.com/cb")
            .await
            .unwrap());
        // A different path is a different registered URI — rejected, no
        // loopback-style leniency for registered clients.
        assert!(!svc
            .validate_client_redirect_uri(&row.id, "https://app.example.com/other")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn validate_client_redirect_uri_unknown_client_is_false_not_error() {
        let svc = service();
        assert!(!svc
            .validate_client_redirect_uri("unknown-client", "https://app.example.com/cb")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn validate_client_redirect_uri_builtin_keeps_loopback_any_port() {
        let svc = service();
        assert!(svc
            .validate_client_redirect_uri(LOCALDB_CLI_CLIENT_ID, "http://127.0.0.1:1/cb")
            .await
            .unwrap());
        assert!(svc
            .validate_client_redirect_uri(LOCALDB_CLI_CLIENT_ID, "http://127.0.0.1:65535/cb")
            .await
            .unwrap());
    }
}
