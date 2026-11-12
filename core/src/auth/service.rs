//! `AuthService`: the policy layer over `AuthStore` (D5).
//!
//! Every method here is orchestration over the trait plus the pure crypto in
//! `token` and grant logic in `principal` — no direct I/O of its own.

mod invites;

use std::sync::Arc;

use crate::ids::new_ulid;
use crate::ingestion::now_rfc3339;
use crate::types::StoreVisibility;
use crate::Error;

use super::client;
use super::principal::{Principal, Role, SessionOrigin, StoreAccess};
use super::store::{
    AccessRequestRow, AccessRequestState, AuthCodeRow, AuthStore, AuthTokenRow, DemoteOutcome,
    InviteMode, InviteRow, OAuthClientRow, RedeemAndIssueCodeOutcome, RegisterClientOutcome,
    RotateOutcome, StoreGrantRow, TokenKind, UserRow,
};
use super::token::{
    hash_secret, is_expired, mint_secret, rfc3339_from_now, verify_pkce_s256, verify_secret,
    ACCESS_TOKEN_TTL_SECS, AUTH_CODE_TTL_SECS, REFRESH_TOKEN_TTL_SECS,
};

/// Hand-written `Debug` for a type shaped `{ row, secret: String }` that
/// redacts `secret` — these types carry a show-once plaintext credential,
/// and a derived `Debug` would print it into logs/panic messages/test
/// failure output verbatim.
macro_rules! redact_secret_in_debug {
    ($ty:ident) => {
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($ty))
                    .field("row", &self.row)
                    .field("secret", &"<redacted>")
                    .finish()
            }
        }
    };
}

/// A newly minted bearer token: the persisted row plus the plaintext secret
/// (shown to the caller exactly once — never persisted, never logged).
#[derive(Clone)]
pub struct IssuedToken {
    pub row: AuthTokenRow,
    pub secret: String,
}
redact_secret_in_debug!(IssuedToken);

/// A newly created invite: the persisted row plus its plaintext secret
/// (shown once).
#[derive(Clone)]
pub struct IssuedInvite {
    pub row: InviteRow,
    pub secret: String,
}
redact_secret_in_debug!(IssuedInvite);

/// A newly minted authorization code: the persisted row plus the plaintext
/// code (shown once, in the `POST /authorize` redirect's `code` param).
#[derive(Clone)]
pub struct IssuedAuthCode {
    pub row: AuthCodeRow,
    pub secret: String,
}
redact_secret_in_debug!(IssuedAuthCode);

/// `AuthService::register_client`'s error type: either the request itself
/// fails this server's DCR policy ([`client::RegistrationError`], pure, no
/// I/O involved), or persisting a validated request hits a store-layer
/// failure. Kept distinct from the shared [`Error`] taxonomy so a caller
/// (`server::auth::register::post_register`) can match on
/// [`Self::Validation`]'s own variant to choose between RFC 7591 §3.2.2's
/// `invalid_redirect_uri`/`invalid_client_metadata` codes, rather than
/// string-matching an `Error::InvalidRequest` message.
#[derive(Debug)]
pub enum RegisterClientError {
    /// The request failed [`client::validate_registration_request`].
    Validation(client::RegistrationError),
    /// A store-layer failure persisting the validated client.
    Store(Error),
    /// `RegistrationPolicy::max_dynamic_clients` is still reached after
    /// pruning unused, expired registrations and evicting the oldest unused
    /// one — every slot is held by a client with a completed authorization
    /// (specs/05-surfaces.md §3.1).
    CapReached,
}

impl From<Error> for RegisterClientError {
    fn from(err: Error) -> Self {
        RegisterClientError::Store(err)
    }
}

impl std::fmt::Display for RegisterClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
            Self::CapReached => write!(
                f,
                "the server is not accepting new client registrations right now; retry later"
            ),
        }
    }
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

    /// Whether a presentation layer should offer initial setup or resume interrupted setup.
    pub async fn bootstrap_needed(&self) -> Result<bool, Error> {
        Ok(self.store.pending_bootstrap_user().await?.is_some()
            || !self.store.admin_exists().await?)
    }

    /// Called only after the presentation layer verifies its local setup code.
    pub async fn begin_bootstrap(&self) -> Result<UserRow, Error> {
        self.store
            .begin_bootstrap(&UserRow {
                id: new_ulid(),
                name: "admin".into(),
                role: Role::Admin,
                created_at: now_rfc3339(),
            })
            .await
    }

    /// An authenticated protected request proves the bootstrap credential was received.
    pub async fn complete_bootstrap(&self, principal: &Principal) -> Result<(), Error> {
        if principal.role == Role::Admin {
            self.store.complete_bootstrap(&principal.user_id).await?;
        }
        Ok(())
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
    /// tracked on every successful `authenticate`. Not tied to any session
    /// — see `issue_api_key_minted_by` for a key minted on behalf of an
    /// active session, which is tainted alongside it on reuse detection.
    pub async fn issue_api_key(&self, user_id: &str) -> Result<IssuedToken, Error> {
        self.issue_api_key_minted_by(user_id, None).await
    }

    /// Mint a long-lived API key, recording `minted_by_family` as the
    /// session family (if any) active when it was minted
    /// (`AuthTokenRow::minted_by_family`). `server::handlers::keys::create_key`
    /// passes the caller's `SessionOrigin` here so a key minted while
    /// authenticated by a session (or by a key already tainted by one) is
    /// revoked alongside it on reuse-detected family revocation.
    pub async fn issue_api_key_minted_by(
        &self,
        user_id: &str,
        minted_by_family: Option<String>,
    ) -> Result<IssuedToken, Error> {
        self.issue_token(
            user_id,
            TokenKind::ApiKey,
            None,
            None,
            None,
            minted_by_family,
        )
        .await
    }

    /// Mint a 1-hour access token (D1), not tied to any session family. Use
    /// `issue_session` for the normal login path, which shares one family
    /// between the access and refresh token so reuse detection revokes
    /// both.
    pub async fn issue_access_token(&self, user_id: &str) -> Result<IssuedToken, Error> {
        self.issue_token(
            user_id,
            TokenKind::Access,
            Some(rfc3339_from_now(ACCESS_TOKEN_TTL_SECS)),
            None,
            None,
            None,
        )
        .await
    }

    /// Mint a 30-day refresh token (D1), starting a new rotation family, not
    /// tied to an access token. Use `issue_session` for the normal login
    /// path.
    pub async fn issue_refresh_token(&self, user_id: &str) -> Result<IssuedToken, Error> {
        let family = new_ulid();
        self.issue_token(
            user_id,
            TokenKind::Refresh,
            Some(rfc3339_from_now(REFRESH_TOKEN_TTL_SECS)),
            Some(family),
            None,
            None,
        )
        .await
    }

    /// Mint a fresh login session (D1): a 1-hour access token and a 30-day
    /// refresh token sharing one new rotation family, so reuse detection on
    /// the refresh token (`rotate_refresh_token`) revokes the access token
    /// minted alongside it too — not just later-rotated replacements. The
    /// OAuth2 token endpoint (`server::auth::oauth::issue_token_pair`) calls
    /// this for the authorization-code grant; every access token a caller
    /// can present either comes from here or from a later
    /// `rotate_refresh_token` call, both of which carry a `family_id`.
    ///
    /// `minted_by_family` is this session's own ancestry: the family of the
    /// credential that authorized the OAuth exchange creating it (the auth
    /// code's own `minted_by_family`, carried by
    /// `server::auth::oauth::handle_auth_code_grant` from
    /// `redeem_auth_code`), or `None` for a login with no session ancestry
    /// (the setup code, an invite redemption, or a plain API key). Both new
    /// tokens carry it, so reuse detection on the ancestor family revokes
    /// this entire descendant session too (`AuthStore::revoke_compromised_family`'s
    /// closure) — and `AuthStore::insert_token` refuses to mint this session
    /// at all if that ancestor was already compromised before the exchange
    /// completed.
    pub async fn issue_session(
        &self,
        user_id: &str,
        minted_by_family: Option<String>,
    ) -> Result<(IssuedToken, IssuedToken), Error> {
        let family = new_ulid();
        let access = self
            .issue_token(
                user_id,
                TokenKind::Access,
                Some(rfc3339_from_now(ACCESS_TOKEN_TTL_SECS)),
                Some(family.clone()),
                None,
                minted_by_family.clone(),
            )
            .await?;
        let refresh = self
            .issue_token(
                user_id,
                TokenKind::Refresh,
                Some(rfc3339_from_now(REFRESH_TOKEN_TTL_SECS)),
                Some(family),
                None,
                minted_by_family,
            )
            .await?;
        Ok((access, refresh))
    }

    async fn issue_token(
        &self,
        user_id: &str,
        kind: TokenKind,
        expires_at: Option<String>,
        family_id: Option<String>,
        rotated_from: Option<String>,
        minted_by_family: Option<String>,
    ) -> Result<IssuedToken, Error> {
        let issued = Self::prepare_token(
            user_id,
            kind,
            expires_at,
            family_id,
            rotated_from,
            minted_by_family,
        );
        self.store.insert_token(&issued.row).await?;
        Ok(issued)
    }

    fn prepare_token(
        user_id: &str,
        kind: TokenKind,
        expires_at: Option<String>,
        family_id: Option<String>,
        rotated_from: Option<String>,
        minted_by_family: Option<String>,
    ) -> IssuedToken {
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
            minted_by_family,
        };
        IssuedToken {
            row,
            secret: minted.secret,
        }
    }

    /// Resolve a bearer secret to a `Principal`: validates the hash,
    /// expiry, and revocation state; updates `last_used_at` for API keys;
    /// and builds `StoreAccess` from the user's role + grants (D7). A thin
    /// wrapper over `authenticate_with_origin` that drops the session
    /// origin — use that directly when the caller needs to know (and
    /// propagate) which session family, if any, backs this credential.
    pub async fn authenticate(&self, bearer_secret: &str) -> Result<Principal, Error> {
        self.authenticate_with_origin(bearer_secret)
            .await
            .map(|(principal, _origin)| principal)
    }

    /// Like `authenticate`, but also resolves the credential's
    /// `SessionOrigin`: the access token's own `family_id` for an
    /// access token, or the API key's `minted_by_family` for an API key —
    /// `None` for a key minted with no session in scope. A key minted by a
    /// principal whose own origin was tainted inherits that same family
    /// (`server::handlers::keys::create_key` passes the caller's
    /// `SessionOrigin` straight through to `issue_api_key_minted_by`), so
    /// the taint propagates transitively through any chain of
    /// session-minted keys.
    pub async fn authenticate_with_origin(
        &self,
        bearer_secret: &str,
    ) -> Result<(Principal, SessionOrigin), Error> {
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

        let origin = SessionOrigin(match token.kind {
            TokenKind::Access => token.family_id.clone(),
            TokenKind::ApiKey => token.minted_by_family.clone(),
            // Refresh tokens are rejected above and never reach this match;
            // `None` rather than a panic keeps this auth path fail-safe if
            // that invariant is ever loosened.
            TokenKind::Refresh => None,
        });

        let access = self.build_store_access(&user).await?;
        Ok((
            Principal {
                user_id: user.id,
                name: user.name,
                role: user.role,
                access,
            },
            origin,
        ))
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

        // Expiry is checked before revocation: only a refresh token that is
        // both already rotated away *and* still unexpired is evidence of
        // reuse. An expired token presented after it has also been rotated
        // away is just a plain stale credential — no family revocation,
        // since nothing could still be replaying it.
        if let Some(expires_at) = &token.expires_at {
            if is_expired(expires_at) {
                return Err(Error::Unauthorized {
                    message: "refresh token has expired".to_string(),
                });
            }
        }

        if token.revoked_at.is_some() {
            // Someone presented a refresh token that was already rotated
            // away (or explicitly revoked) — treat as theft and burn the
            // whole family, plus any API key minted under it, so a
            // stolen session can't be replayed indefinitely or escalated
            // into a permanent key.
            if let Some(family) = &token.family_id {
                self.store.revoke_compromised_family(family).await?;
            }
            return Err(Error::Unauthorized {
                message: "refresh token reuse detected; session revoked".to_string(),
            });
        }

        let family = token.family_id.clone().unwrap_or_else(new_ulid);
        // Both replacements inherit the rotated-away token's own ancestry
        // (`minted_by_family`), so every row of a session created by
        // authorizing with another session's credential keeps carrying that
        // ancestor forward through every rotation — reuse detection on the
        // ancestor's family still reaches this session's current tokens,
        // however many times it has rotated since.
        let new_refresh = Self::prepare_token(
            &token.user_id,
            TokenKind::Refresh,
            Some(rfc3339_from_now(REFRESH_TOKEN_TTL_SECS)),
            Some(family.clone()),
            Some(token.id.clone()),
            token.minted_by_family.clone(),
        );
        // The rotated access token joins the same family as the refresh
        // token it was minted alongside, so reuse detection below (and any
        // later rotation in this chain) revokes it too — not just the
        // refresh token and later replacements.
        let new_access = Self::prepare_token(
            &token.user_id,
            TokenKind::Access,
            Some(rfc3339_from_now(ACCESS_TOKEN_TTL_SECS)),
            Some(family.clone()),
            None,
            token.minted_by_family.clone(),
        );
        match self
            .store
            .rotate_tokens(&token.id, &new_access.row, &new_refresh.row)
            .await?
        {
            RotateOutcome::Rotated => Ok((new_access, new_refresh)),
            RotateOutcome::AlreadyRotated => {
                self.store.revoke_compromised_family(&family).await?;
                Err(Error::Unauthorized {
                    message: "refresh token reuse detected; session revoked".into(),
                })
            }
            RotateOutcome::Ineligible => Err(Error::Unauthorized {
                message: "refresh token is not eligible for rotation".into(),
            }),
        }
    }

    /// Mint a single-use OAuth2 authorization code (RFC 6749 §4.1), bound to
    /// `client_id` + `redirect_uri` + PKCE `code_challenge` at issue time
    /// and expiring in [`AUTH_CODE_TTL_SECS`] (specs/05-surfaces.md
    /// §3.1 R5). Redirect-uri validation policy is the caller's
    /// responsibility (`core::auth::validate_redirect_uri`, checked by
    /// `server/src/auth/oauth.rs` before calling this).
    ///
    /// `minted_by_family` is the session family (if any) of the credential
    /// that authorized this `/authorize` request — `server::auth::oauth`
    /// resolves it via `authenticate_with_origin` for the setup-code/API-key
    /// branch, and passes `None` for the setup-code branch itself and for an
    /// invite redemption, both of which have no session ancestry. Carried
    /// on the code so `redeem_auth_code` can hand it to the session this
    /// code ultimately mints.
    pub async fn issue_auth_code(
        &self,
        client_id: &str,
        user_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
        minted_by_family: Option<String>,
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
            minted_by_family,
        };
        self.store.create_auth_code(&row).await?;
        Ok(IssuedAuthCode {
            row,
            secret: minted.secret,
        })
    }

    /// Whether `client_id` is recognized: the built-in `localdb-cli` public
    /// client (pure, no store lookup — `client::is_known_client`) or a
    /// dynamically registered client (`POST /register`) found in the
    /// `oauth_clients` table.
    pub async fn is_known_client(&self, client_id: &str) -> Result<bool, Error> {
        if client::is_known_client(client_id) {
            return Ok(true);
        }
        Ok(self.store.find_oauth_client(client_id).await?.is_some())
    }

    /// Validate `redirect_uri` for `client_id`, extending
    /// `client::validate_redirect_uri`: the built-in `localdb-cli` client
    /// keeps its RFC 8252 §7.3 loopback-any-port
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

    /// Dynamic Client Registration (RFC 7591): register a new public
    /// client with the given `redirect_uris` (validated one-by-one via
    /// `client::validate_registration_redirect_uri` — exact `https://` or
    /// loopback `http://` only, see that function's doc comment for the
    /// custom-scheme rejection rationale) and an optional display
    /// `client_name`. Mints a ULID `client_id`; there is no client secret
    /// (public clients only, mirroring `localdb-cli`'s own policy).
    ///
    /// `token_endpoint_auth_method`/`grant_types`/`response_types` are
    /// validated (via [`client::validate_registration_request`]) but never
    /// persisted — `OAuthClientRow` has no columns for them; a `201`
    /// response re-derives them from the request (or this server's
    /// defaults) rather than reading them back from storage.
    ///
    /// Enforces [`client::validate_registration_request`] — the one DCR
    /// validator — before persisting anything; `server::auth::register::
    /// post_register` performs no validation of its own, only mapping each
    /// [`client::RegistrationError`] variant onto its RFC 7591 §3.2.2 error
    /// code.
    ///
    /// `policy: RegistrationPolicy::Limited` prunes dynamically registered
    /// clients older than `unused_client_ttl_secs` that have never
    /// completed an authorization before inserting; if
    /// `max_dynamic_clients` is still reached, evicts the single oldest
    /// remaining unused client to make room, and only refuses with
    /// [`RegisterClientError::CapReached`] if every slot is held by a
    /// client with a completed authorization — see [`AuthStore::
    /// prune_and_create_oauth_client`] for why this runs as one transaction
    /// with the insert. `policy: RegistrationPolicy::Disabled` (used when
    /// `server.abuse_limits.enable` is `false`) skips all of that: a plain
    /// insert that can never prune, evict, or refuse.
    pub async fn register_client(
        &self,
        redirect_uris: Vec<String>,
        client_name: Option<String>,
        token_endpoint_auth_method: Option<&str>,
        grant_types: Option<&[String]>,
        response_types: Option<&[String]>,
        policy: &client::RegistrationPolicy,
    ) -> Result<OAuthClientRow, RegisterClientError> {
        client::validate_registration_request(
            &redirect_uris,
            client_name.as_deref(),
            token_endpoint_auth_method,
            grant_types,
            response_types,
        )
        .map_err(RegisterClientError::Validation)?;
        // `rfc3339_from_now(0)`, not `now_rfc3339()`: the latter is frozen
        // under `#[cfg(test)]` (deterministic document/resource fixtures),
        // which would make this row's `created_at` incomparable against
        // `unused_before` below — both must come from the same clock so the
        // string comparison `prune_and_create_oauth_client` does stays
        // meaningful in tests, not just in production (where the two
        // functions agree anyway).
        let row = OAuthClientRow {
            id: new_ulid(),
            client_name,
            redirect_uris,
            created_at: rfc3339_from_now(0),
        };
        match policy {
            client::RegistrationPolicy::Disabled => {
                self.store.create_oauth_client(&row).await?;
                Ok(row)
            }
            client::RegistrationPolicy::Limited {
                max_dynamic_clients,
                unused_client_ttl_secs,
            } => {
                let unused_before = rfc3339_from_now(-unused_client_ttl_secs);
                match self
                    .store
                    .prune_and_create_oauth_client(
                        &row,
                        client::LOCALDB_CLI_CLIENT_ID,
                        &unused_before,
                        *max_dynamic_clients,
                    )
                    .await?
                {
                    RegisterClientOutcome::Created => Ok(row),
                    RegisterClientOutcome::CapReached => Err(RegisterClientError::CapReached),
                }
            }
        }
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
    ///
    /// Returns the code's own `minted_by_family` alongside the user, so the
    /// caller (`server::auth::oauth::handle_auth_code_grant`) can pass it to
    /// `issue_session`, carrying the authorizing credential's session
    /// ancestry forward onto the new session.
    pub async fn redeem_auth_code(
        &self,
        code: &str,
        client_id: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<(UserRow, Option<String>), Error> {
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

        let user = self
            .store
            .get_user(&row.user_id)
            .await?
            .ok_or_else(|| Error::Unauthorized {
                message: "authorization code's user no longer exists".to_string(),
            })?;
        Ok((user, row.minted_by_family))
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
    /// **Concurrency:** the guard is enforced atomically at
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
    /// **Concurrency:** demotion to `member` is enforced entirely by the
    /// single guarded write in `AuthStore::try_demote_user_unless_last_admin`
    /// — unlike `delete_user`, there is no second, unguarded fallback write
    /// afterward (that fallback used to let a concurrent promote/demote race
    /// its own re-check and leave zero admins). The store distinguishes
    /// "demoted or already a member" from "unknown id" from "is the sole
    /// admin" directly (`DemoteOutcome`), so this method only has to map
    /// each to its own result.
    pub async fn set_user_role(&self, id: &str, role: Role) -> Result<(), Error> {
        if role != Role::Member {
            return self.store.update_user_role(id, role).await;
        }
        match self.store.try_demote_user_unless_last_admin(id).await? {
            DemoteOutcome::Done => Ok(()),
            DemoteOutcome::LastAdmin => Err(Error::InvalidRequest {
                message: "cannot demote the last remaining admin account".to_string(),
            }),
            DemoteOutcome::NotFound => Err(Error::InvalidRequest {
                message: format!("user '{id}' not found"),
            }),
        }
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

/// Outcome of `AuthService::redeem_invite`.
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

/// Outcome of `AuthService::redeem_invite_for_authorize` — the OAuth
/// consent page's invite branch only (`server::auth::oauth::
/// handle_invite_authorize`); `AuthService::redeem_invite`'s own
/// `RedeemOutcome` is unaffected and keeps its existing behavior for
/// `POST /v1/invites/redeem` and `/join`.
#[derive(Debug, Clone)]
pub enum AuthorizeInviteOutcome {
    /// `open`-mode invite: the new user, its store grants (echoed back for
    /// display), and the first authorization code issued to it, minted
    /// together with the redemption as one transaction.
    Issued {
        user: UserRow,
        grants: Vec<String>,
        issued: Box<IssuedAuthCode>,
    },
    /// `closed`-mode invite: identical to `RedeemOutcome::Closed` — a
    /// pending access request was filed; no code is issued until an admin
    /// later approves it.
    Closed {
        request_id: String,
        request_secret: String,
    },
    /// `client_id` is not the built-in client and has no row in
    /// `oauth_clients` — evicted (by a concurrent registration at the
    /// dynamic-client cap) or never registered. Nothing committed: not the
    /// mint, not the invite's use. The caller should treat this exactly
    /// like an unknown `client_id` at `/authorize` validation time.
    UnknownClient,
}

/// Outcome of `AuthService::poll_request`: invites redeem without a bearer
/// token, and a `closed`-mode invite resolves through this request/poll
/// flow rather than completing immediately.
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
mod tests;
