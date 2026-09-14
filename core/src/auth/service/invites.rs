use super::*;

impl<S: AuthStore> AuthService<S> {
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
    /// **Concurrency (documented choice):** a use is *reserved* atomically
    /// (`AuthStore::try_consume_invite_use`, an `UPDATE ... WHERE uses <
    /// max_uses` conditional update) before the mint (user-create /
    /// access-request-file) is attempted, and *released*
    /// (`AuthStore::release_invite_use`) if that mint then fails. This
    /// reserve-then-release ordering satisfies both invariants at once:
    ///
    /// 1. **Never over-redeem:** because the reservation is an atomic
    ///    conditional update, concurrent redemptions — even under distinct
    ///    `requested_name`s, which the `users.name` UNIQUE constraint alone
    ///    cannot help with — can never together push `uses` past
    ///    `max_uses`. A racer that arrives after the invite is exhausted
    ///    gets `Unauthorized` "invite has no remaining uses" immediately,
    ///    before attempting any mint.
    /// 2. **Never burn a use on a failed redemption:** if the reservation
    ///    succeeds but the subsequent mint fails (most commonly a duplicate
    ///    `requested_name` racing `create_user`'s UNIQUE constraint), the
    ///    reserved slot is released before the error is returned — so a
    ///    caller can retry with a different name against the same
    ///    `max_uses = 1` invite without it reading as exhausted.
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

        // Reserve a use atomically before attempting the mint — see the doc
        // comment above for why this ordering (rather than incrementing
        // after a successful mint, or before any check at all) is what
        // makes both concurrency invariants hold simultaneously.
        if !self.store.try_consume_invite_use(&invite.id).await? {
            return Err(Error::Unauthorized {
                message: "invite has no remaining uses".to_string(),
            });
        }

        let outcome = match invite.mode {
            InviteMode::Open => {
                self.mint_open_invite_redemption(&invite, requested_name)
                    .await
            }
            InviteMode::Closed => {
                self.mint_closed_invite_redemption(&invite, requested_name)
                    .await
            }
        };

        if outcome.is_err() {
            // The mint failed after we reserved a use — release it so this
            // failed redemption doesn't permanently consume a slot. Best
            // effort: if the release itself errors, we still surface the
            // original mint error (e.g. "user already exists") rather than
            // masking it with an unrelated store error.
            let _ = self.store.release_invite_use(&invite.id).await;
        }

        outcome
    }

    /// `open`-mode mint half of `redeem_invite`: create the user, apply the
    /// invite's store grants, and issue a show-once API-key credential.
    ///
    /// **Orphan cleanup (finding #6 fix):** `AuthStore` has no multi-call
    /// transaction seam, so a failure in either post-create step
    /// (`apply_invite_grants` — e.g. a store deleted after the invite was
    /// created — or `issue_api_key`) would otherwise leave a real, if
    /// uncredentialed, user row behind: `redeem_invite` releases the
    /// reserved invite use on any error here, but that alone doesn't remove
    /// the user, so a retry with the same `requested_name` would then fail
    /// "user already exists" forever. Instead, once `create_user` has
    /// succeeded, any later failure triggers a best-effort compensating
    /// `delete_user` (the plain, unconditional store method — this is a
    /// fresh `Role::Member` row, never subject to the last-admin guard)
    /// before the original error is surfaced, so the caller's next attempt
    /// starts from a clean slate.
    async fn mint_open_invite_redemption(
        &self,
        invite: &InviteRow,
        requested_name: &str,
    ) -> Result<RedeemOutcome, Error> {
        let user = self.create_user(requested_name, Role::Member).await?;

        if let Err(err) = self.apply_invite_grants(invite, &user.id).await {
            let _ = self.store.delete_user(&user.id).await;
            return Err(err);
        }
        let credential = match self.issue_api_key(&user.id).await {
            Ok(credential) => credential,
            Err(err) => {
                let _ = self.store.delete_user(&user.id).await;
                return Err(err);
            }
        };

        Ok(RedeemOutcome::Open {
            user,
            grants: invite.store_grants.clone(),
            credential: Box::new(credential),
        })
    }

    /// `closed`-mode mint half of `redeem_invite`: file a pending
    /// `AccessRequestRow` awaiting admin approval.
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
        self.store.create_access_request(&request).await?;
        Ok(RedeemOutcome::Closed {
            request_id: request.id,
            request_secret: minted.secret,
        })
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
    ///
    /// **Concurrency (finding #4 fix):** the initial `state != Pending`
    /// check above is only a fast-path rejection — it cannot by itself
    /// prevent two concurrent decisions (an `approve_request` racing a
    /// `deny_request`, or either racing a duplicate call) from both passing
    /// the check and then both writing. The actual guard is
    /// `AuthStore::try_decide_access_request`, an atomic `UPDATE ... WHERE
    /// state = 'pending'` performed *after* the user has been created and
    /// granted: whichever caller's conditional update lands first wins the
    /// decision, and the loser's `try_decide_access_request` reports
    /// `false`. Because the user is created *before* claiming the decision
    /// (there is no way to know its id otherwise), a losing `approve_request`
    /// would otherwise leave a real user with live grants attached to a
    /// request that ended up `denied` — so on `false` we best-effort
    /// compensating-delete the user we just created (mirroring
    /// `mint_open_invite_redemption`'s finding #6 cleanup) before surfacing
    /// the conflict. This guarantees a denied request can never have a live
    /// resulting user, and exactly one of a racing approve/deny pair takes
    /// effect.
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

        let decided = self
            .store
            .try_decide_access_request(
                request_id,
                AccessRequestState::Approved,
                Some(&user.id),
                &now_rfc3339(),
            )
            .await?;
        if !decided {
            // Lost the race: a concurrent approve/deny already decided this
            // request first. Don't leave the user we just created (with
            // grants already applied) dangling off a request that isn't
            // `approved` — best-effort delete it, then surface the
            // conflict. `self.store.delete_user` (not `self.delete_user`)
            // is deliberate: this is a fresh `Role::Member` row, never
            // subject to the last-admin guard.
            let _ = self.store.delete_user(&user.id).await;
            return Err(Error::InvalidRequest {
                message: format!("access request '{request_id}' is no longer pending"),
            });
        }

        Ok(user)
    }

    /// Deny a pending closed-mode access request (T6). No user is created;
    /// the requester's next `poll_request` observes `PollOutcome::Denied`.
    ///
    /// **Concurrency (finding #4 fix):** same atomic guard as
    /// `approve_request` — see that method's doc comment. A denial that
    /// loses the race to a concurrent decision reports the same "no longer
    /// pending" conflict rather than silently overwriting whatever the
    /// winner decided.
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
    /// `PollOutcome::AlreadyCollected` instead of a credential.
    ///
    /// **Mint-then-mark ordering (finding #7 fix):** the API key is minted
    /// *before* the collection is marked, not after. Marking collected
    /// first (the original ordering) meant a subsequent `issue_api_key`
    /// failure — the user deleted between approval and poll, or a
    /// transient store error — left `collected_at` set with no credential
    /// ever having been handed out, permanently bricking collection (every
    /// later poll would see `AlreadyCollected` forever). With minting
    /// first: if the mint fails, `collected_at` is never touched, so the
    /// request remains collectible and a later poll can retry. If the mint
    /// succeeds but this poll then loses the atomic collect race to a
    /// concurrent one, the freshly minted key is best-effort revoked before
    /// reporting `AlreadyCollected`, so a losing racer never leaves a live,
    /// never-handed-out credential behind — the exactly-once delivery
    /// invariant (at most one *usable* key reaches a caller) holds
    /// alongside the new liveness invariant (a mint failure doesn't
    /// permanently brick collection).
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

                // Mint first (see doc comment): a failure here leaves
                // `collected_at` untouched, so this request stays
                // collectible for a retry.
                let issued = self.issue_api_key(user_id).await?;

                let collected = self
                    .store
                    .mark_access_request_collected(request_id, &now_rfc3339())
                    .await?;
                if collected {
                    Ok(PollOutcome::Approved {
                        credential: issued.secret,
                    })
                } else {
                    // Lost the race to a concurrent poll that collected
                    // first — don't leave the key we just minted live and
                    // unclaimed.
                    let _ = self.store.revoke_token(&issued.row.id).await;
                    Ok(PollOutcome::AlreadyCollected)
                }
            }
        }
    }
}
