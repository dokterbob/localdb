//! `AuthService`: the policy layer over `AuthStore` (D5).
//!
//! Every method here is orchestration over the trait plus the pure crypto in
//! `token` and grant logic in `principal` — no direct I/O of its own.

use std::sync::Arc;

use crate::ids::new_ulid;
use crate::ingestion::now_rfc3339;
use crate::types::StoreVisibility;
use crate::Error;

use super::principal::{Principal, Role, StoreAccess};
use super::store::{
    AuthStore, AuthTokenRow, InviteMode, InviteRow, StoreGrantRow, TokenKind, UserRow,
};
use super::token::{
    hash_secret, is_expired, mint_secret, rfc3339_from_now, ACCESS_TOKEN_TTL_SECS,
    REFRESH_TOKEN_TTL_SECS,
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

    /// D7 grant evaluation, delegated to the pure logic on `Principal`.
    pub fn can_read_store(
        &self,
        principal: &Principal,
        store_name: &str,
        visibility: StoreVisibility,
    ) -> bool {
        principal.can_read_store(store_name, visibility)
    }

    /// Create an invite. T1 ships the table plus minting; the redeem/approve
    /// state machine is T6.
    pub async fn create_invite(
        &self,
        mode: InviteMode,
        store_grants: Vec<String>,
        max_uses: u32,
        expires_at: Option<String>,
        created_by: &str,
    ) -> Result<IssuedInvite, Error> {
        let minted = mint_secret();
        let row = InviteRow {
            id: new_ulid(),
            token_hash: minted.hash,
            mode,
            store_grants,
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
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn create_invite_mints_a_show_once_secret() {
        let svc = service();
        let issued = svc
            .create_invite(
                InviteMode::Open,
                vec!["docs".to_string()],
                1,
                None,
                "admin-1",
            )
            .await
            .unwrap();
        assert!(issued.secret.starts_with(TOKEN_PREFIX));
        assert_eq!(issued.row.uses, 0);
        assert_eq!(issued.row.max_uses, 1);
    }
}
