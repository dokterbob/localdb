use super::*;

impl<S: AuthStore> AuthService<S> {
    /// Create an invite: `mode` (open/closed), `store_grants` (name +
    /// visibility pairs — the caller resolves visibility via `StoreBackend`,
    /// the same seam `grant_store` uses), `max_uses` (>= 1), and an optional
    /// absolute RFC 3339 `expires_at`.
    ///
    /// Grants against a `private` store are rejected here at CREATE time
    /// (`Forbidden`, reusing `grant_store`'s rule that private stores are
    /// admin-only and ungrantable) rather than deferred to
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
        let expires_at = expires_at
            .map(|value| {
                chrono::DateTime::parse_from_rfc3339(&value)
                    .map(|time| {
                        time.with_timezone(&chrono::Utc)
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    })
                    .map_err(|_| Error::InvalidRequest {
                        message: "expires_at must be an RFC 3339 timestamp".into(),
                    })
            })
            .transpose()?;
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
    fn invite_grants(invite: &InviteRow, user_id: &str) -> Vec<StoreGrantRow> {
        invite
            .store_grants
            .iter()
            .map(|name| StoreGrantRow {
                store_name: name.clone(),
                user_id: user_id.to_string(),
                granted_by: invite.created_by.clone(),
                created_at: now_rfc3339(),
            })
            .collect()
    }

    /// Redeem an invite token (D9): resolves the presented secret to an
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
    /// **Concurrency (documented choice):** reserving a use and committing
    /// the mint (user-create-and-grant, or access-request-file) are one
    /// transaction (`AuthStore::redeem_open_invite`/`redeem_closed_invite`),
    /// not two separate store calls. This satisfies both invariants at
    /// once:
    ///
    /// 1. **Never over-redeem:** the reservation is part of the same atomic
    ///    conditional update either way, so concurrent redemptions — even
    ///    under distinct `requested_name`s, which the `users.name` UNIQUE
    ///    constraint alone cannot help with — can never together push
    ///    `uses` past `max_uses`. A racer that arrives after the invite is
    ///    exhausted gets `Unauthorized` "invite has no remaining uses".
    /// 2. **Never burn a use on a failed redemption:** if the reservation
    ///    succeeds but the rest of the transaction fails (most commonly a
    ///    duplicate `requested_name` racing `create_user`'s UNIQUE
    ///    constraint), the whole transaction — reservation included — rolls
    ///    back, so a caller can retry with a different name against the
    ///    same `max_uses = 1` invite without it reading as exhausted.
    pub async fn redeem_invite(
        &self,
        token_secret: &str,
        requested_name: &str,
    ) -> Result<RedeemOutcome, Error> {
        let invite = self
            .resolve_live_invite(token_secret, requested_name)
            .await?;
        match invite.mode {
            InviteMode::Open => {
                self.mint_open_invite_redemption(&invite, requested_name)
                    .await
            }
            InviteMode::Closed => {
                self.mint_closed_invite_redemption(&invite, requested_name)
                    .await
            }
        }
    }

    /// Like `redeem_invite`, but for the OAuth consent page's invite branch
    /// (`server::auth::oauth::handle_invite_authorize`) only: an
    /// `open`-mode redemption's mint and its first authorization code
    /// commit as one transaction (`AuthStore::
    /// redeem_open_invite_and_issue_code`), closing the race a separate
    /// `redeem_invite` call followed by a separate `issue_auth_code` call
    /// leaves open — see that store method's doc comment. A `closed`-mode
    /// redemption behaves exactly like `redeem_invite`: no code is issued
    /// until an admin later approves the request.
    ///
    /// `client_id`/`redirect_uri`/`code_challenge` are the same
    /// `/authorize` parameters `AuthService::issue_auth_code` would
    /// otherwise be called with for this user — the caller has already
    /// validated them (`server::auth::oauth::validate_authorize_params`)
    /// before reaching this invite branch at all.
    pub async fn redeem_invite_for_authorize(
        &self,
        token_secret: &str,
        requested_name: &str,
        client_id: &str,
        redirect_uri: &str,
        code_challenge: &str,
    ) -> Result<AuthorizeInviteOutcome, Error> {
        let invite = self
            .resolve_live_invite(token_secret, requested_name)
            .await?;
        match invite.mode {
            InviteMode::Closed => match self
                .mint_closed_invite_redemption(&invite, requested_name)
                .await?
            {
                RedeemOutcome::Closed {
                    request_id,
                    request_secret,
                } => Ok(AuthorizeInviteOutcome::Closed {
                    request_id,
                    request_secret,
                }),
                RedeemOutcome::Open { .. } => unreachable!(
                    "mint_closed_invite_redemption on a Closed-mode invite never returns Open"
                ),
            },
            InviteMode::Open => {
                let user = UserRow {
                    id: new_ulid(),
                    name: requested_name.to_string(),
                    role: Role::Member,
                    created_at: now_rfc3339(),
                };
                let grants = Self::invite_grants(&invite, &user.id);
                let credential =
                    Self::prepare_token(&user.id, TokenKind::ApiKey, None, None, None, None);
                let minted = mint_secret();
                let auth_code = AuthCodeRow {
                    id: new_ulid(),
                    client_id: client_id.to_string(),
                    user_id: user.id.clone(),
                    code_hash: minted.hash,
                    code_challenge: code_challenge.to_string(),
                    code_challenge_method: "S256".to_string(),
                    redirect_uri: redirect_uri.to_string(),
                    expires_at: rfc3339_from_now(AUTH_CODE_TTL_SECS),
                    consumed_at: None,
                    created_at: now_rfc3339(),
                    minted_by_family: None,
                };
                match self
                    .store
                    .redeem_open_invite_and_issue_code(
                        &invite.id,
                        &user,
                        &grants,
                        &credential.row,
                        client::LOCALDB_CLI_CLIENT_ID,
                        &auth_code,
                    )
                    .await?
                {
                    RedeemAndIssueCodeOutcome::Issued => Ok(AuthorizeInviteOutcome::Issued {
                        user,
                        grants: invite.store_grants.clone(),
                        issued: Box::new(IssuedAuthCode {
                            row: auth_code,
                            secret: minted.secret,
                        }),
                    }),
                    RedeemAndIssueCodeOutcome::NotEligible => Err(Error::Unauthorized {
                        message: "invite has no remaining uses".to_string(),
                    }),
                    RedeemAndIssueCodeOutcome::UnknownClient => {
                        Ok(AuthorizeInviteOutcome::UnknownClient)
                    }
                }
            }
        }
    }

    /// Shared preamble of `redeem_invite`/`redeem_invite_for_authorize`:
    /// resolve the presented secret to a still-live `InviteRow` (known,
    /// not revoked, not expired). `uses < max_uses` is not checked here —
    /// it is part of the same atomic reservation each caller's own mint
    /// performs.
    async fn resolve_live_invite(
        &self,
        token_secret: &str,
        requested_name: &str,
    ) -> Result<InviteRow, Error> {
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
        Ok(invite)
    }

    /// `open`-mode mint half of `redeem_invite`: reserve a use, create the
    /// user, apply the invite's store grants, and issue a show-once
    /// API-key credential, all as one transaction
    /// (`AuthStore::redeem_open_invite`) — a failure anywhere in that mint
    /// (most commonly a duplicate `requested_name`) rolls everything back,
    /// so no partial user or burned use is ever left behind.
    async fn mint_open_invite_redemption(
        &self,
        invite: &InviteRow,
        requested_name: &str,
    ) -> Result<RedeemOutcome, Error> {
        let user = UserRow {
            id: new_ulid(),
            name: requested_name.to_string(),
            role: Role::Member,
            created_at: now_rfc3339(),
        };
        let grants = Self::invite_grants(invite, &user.id);
        let credential = Self::prepare_token(&user.id, TokenKind::ApiKey, None, None, None, None);

        if !self
            .store
            .redeem_open_invite(&invite.id, &user, &grants, &credential.row)
            .await?
        {
            return Err(Error::Unauthorized {
                message: "invite has no remaining uses".to_string(),
            });
        }

        Ok(RedeemOutcome::Open {
            user,
            grants: invite.store_grants.clone(),
            credential: Box::new(credential),
        })
    }

    /// `closed`-mode mint half of `redeem_invite`: reserve a use and file a
    /// pending `AccessRequestRow` awaiting admin approval, together as one
    /// transaction (`AuthStore::redeem_closed_invite`).
    async fn mint_closed_invite_redemption(
        &self,
        invite: &InviteRow,
        requested_name: &str,
    ) -> Result<RedeemOutcome, Error> {
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
        if !self
            .store
            .redeem_closed_invite(&invite.id, &request)
            .await?
        {
            return Err(Error::Unauthorized {
                message: "invite has no remaining uses".to_string(),
            });
        }
        Ok(RedeemOutcome::Closed {
            request_id: request.id,
            request_secret: minted.secret,
        })
    }

    /// Look up `request_id` and confirm it is still `Pending` — the shared
    /// preamble of `approve_request` and `deny_request`, which otherwise
    /// differ only in which decision they apply.
    async fn find_pending_request(&self, request_id: &str) -> Result<AccessRequestRow, Error> {
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
        Ok(request)
    }

    /// Approve a pending closed-mode access request: creates the user
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
    ///
    /// User creation, grants, and the conditional decision commit together.
    /// A failed grant or a concurrent decision leaves no partial user behind.
    pub async fn approve_request(&self, request_id: &str) -> Result<UserRow, Error> {
        let request = self.find_pending_request(request_id).await?;
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

        let user = UserRow {
            id: new_ulid(),
            name: request.requested_name.clone(),
            role: Role::Member,
            created_at: now_rfc3339(),
        };
        let grants = Self::invite_grants(&invite, &user.id);
        if !self
            .store
            .approve_access_request(request_id, &user, &grants)
            .await?
        {
            return Err(Error::InvalidRequest {
                message: format!("access request '{request_id}' is no longer pending"),
            });
        }

        Ok(user)
    }

    /// Deny a pending closed-mode access request. No user is created;
    /// the requester's next `poll_request` observes `PollOutcome::Denied`.
    ///
    /// **Concurrency:** same atomic guard as `approve_request` — see that
    /// method's doc comment. A denial that loses the race to a concurrent
    /// decision reports the same "no longer pending" conflict rather than
    /// silently overwriting whatever the winner decided.
    pub async fn deny_request(&self, request_id: &str) -> Result<(), Error> {
        self.find_pending_request(request_id).await?;
        let decided = self
            .store
            .try_decide_access_request(request_id, AccessRequestState::Denied, None, &now_rfc3339())
            .await?;
        if !decided {
            return Err(Error::InvalidRequest {
                message: format!("access request '{request_id}' is no longer pending"),
            });
        }
        Ok(())
    }

    /// Poll a closed-mode access request's status (device-authorization
    /// -grant pattern, RFC 8628). `presented_secret` must match the
    /// request's own `secret_hash` — an unknown `request_id` and a wrong
    /// secret are deliberately indistinguishable (`Unauthorized`, same
    /// message), so a caller cannot use this endpoint to enumerate valid
    /// request IDs (no existence oracle; specs/05-surfaces.md §3.1).
    ///
    /// The request secret is poll-only — it is never promoted to a
    /// credential. It travels in the poll request's JSON body, not the URL
    /// (a URL is routinely recorded by reverse-proxy and access logs, and a
    /// secret that leaked through one would let a log reader collect the
    /// minted key ahead of the real requester), but the body is sent on
    /// every poll either way, so it still isn't fit to serve as a
    /// long-lived credential. Instead, on the transition into `Approved`, a
    /// *fresh* API key is minted for the request's resulting user and
    /// handed back exactly once: marking the request collected and
    /// inserting that key commit together, as one transaction
    /// (`AuthStore::collect_access_request`, mirroring
    /// `consume_auth_code`'s single-use guard), so a second successful poll
    /// (or two concurrent ones racing the first) observes
    /// `PollOutcome::AlreadyCollected` instead of a credential — and a
    /// losing racer never has anything minted for it in the first place,
    /// since its conditional update affects nothing and the whole
    /// transaction rolls back.
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

                let credential =
                    Self::prepare_token(user_id, TokenKind::ApiKey, None, None, None, None);
                let collected = self
                    .store
                    .collect_access_request(request_id, &now_rfc3339(), &credential.row)
                    .await?;
                if collected {
                    Ok(PollOutcome::Approved {
                        credential: credential.secret,
                    })
                } else {
                    Ok(PollOutcome::AlreadyCollected)
                }
            }
        }
    }
}
