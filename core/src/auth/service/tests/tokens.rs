use super::*;

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
        minted_by_family: None,
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
async fn expired_and_revoked_refresh_token_is_plain_401_and_does_not_revoke_the_family() {
    // Policy: only a refresh token that was already rotated away AND is
    // still unexpired is reuse evidence. A stale refresh token that is both
    // already-rotated and expired is a plain 401 — expiry is checked first,
    // so the family (and its still-live refresh/access tokens) is never
    // touched.
    let svc = service();
    let user = svc.create_user("yara", Role::Admin).await.unwrap();
    let (live_access, live_refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = live_refresh.row.family_id.clone().unwrap();

    let minted = mint_secret();
    let stale = AuthTokenRow {
        id: new_ulid(),
        user_id: user.id.clone(),
        kind: TokenKind::Refresh,
        secret_hash: minted.hash.clone(),
        expires_at: Some(rfc3339_from_now(-10)),
        last_used_at: None,
        revoked_at: Some(rfc3339_from_now(-5)),
        created_at: now_rfc3339(),
        family_id: Some(family),
        rotated_from: None,
        minted_by_family: None,
    };
    svc.store.insert_token(&stale).await.unwrap();

    let err = svc.rotate_refresh_token(&minted.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    // Neither the family's live access token nor its live refresh token was
    // touched by the stale token's rejection.
    assert!(svc.authenticate(&live_access.secret).await.is_ok());
    assert!(svc.rotate_refresh_token(&live_refresh.secret).await.is_ok());
}

#[tokio::test]
async fn reuse_revokes_initial_and_rotated_access_tokens() {
    // A login's very first access token (minted by `issue_session`
    // alongside the refresh token) must be revoked on reuse detection, not
    // just later-rotated replacements.
    let svc = service();
    let user = svc.create_user("quentin", Role::Admin).await.unwrap();
    let (initial_access, refresh) = svc.issue_session(&user.id, None).await.unwrap();
    assert_eq!(initial_access.row.family_id, refresh.row.family_id);

    let (rotated_access, rotated_refresh) =
        svc.rotate_refresh_token(&refresh.secret).await.unwrap();

    // Reuse the old (now-rotated-away) refresh secret — theft.
    let err = svc.rotate_refresh_token(&refresh.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    assert!(
        svc.authenticate(&initial_access.secret).await.is_err(),
        "the initial login's access token must be revoked, not just later rotations"
    );
    assert!(svc.authenticate(&rotated_access.secret).await.is_err());
    assert!(svc
        .rotate_refresh_token(&rotated_refresh.secret)
        .await
        .is_err());
}

#[tokio::test]
async fn reuse_revokes_tainted_api_keys_transitively_but_not_untainted_ones() {
    let svc = service();
    let user = svc.create_user("rosa", Role::Admin).await.unwrap();
    let (_access, refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = refresh.row.family_id.clone().unwrap();

    // A key minted directly under the session family.
    let tainted = svc
        .issue_api_key_minted_by(&user.id, Some(family))
        .await
        .unwrap();
    // A key minted by a principal whose own origin IS that tainted key —
    // i.e. a key derived from it — must inherit the taint transitively.
    let (_principal, origin) = svc.authenticate_with_origin(&tainted.secret).await.unwrap();
    let derived = svc
        .issue_api_key_minted_by(&user.id, origin.0.clone())
        .await
        .unwrap();
    // An untainted key, minted with no session in scope.
    let untainted = svc.issue_api_key(&user.id).await.unwrap();

    let (_new_access, _new_refresh) = svc.rotate_refresh_token(&refresh.secret).await.unwrap();
    let err = svc.rotate_refresh_token(&refresh.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    assert!(svc.authenticate(&tainted.secret).await.is_err());
    assert!(
        svc.authenticate(&derived.secret).await.is_err(),
        "a key minted by a tainted principal must inherit the taint"
    );
    assert!(svc.authenticate(&untainted.secret).await.is_ok());
}

#[tokio::test]
async fn revoke_by_secret_on_refresh_revokes_access_but_not_a_tainted_key() {
    // Logout (RFC 7009) ends the session's own access/refresh tokens, never
    // a key the user minted on purpose — even one minted under that same
    // session. `revoke_compromised_family` is reuse-detection-only.
    let svc = service();
    let user = svc.create_user("sam", Role::Admin).await.unwrap();
    let (access, refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = refresh.row.family_id.clone().unwrap();
    let tainted = svc
        .issue_api_key_minted_by(&user.id, Some(family))
        .await
        .unwrap();

    assert!(svc.revoke_by_secret(&refresh.secret).await.unwrap());

    assert!(svc.authenticate(&access.secret).await.is_err());
    assert!(svc.authenticate(&tainted.secret).await.is_ok());
}

#[tokio::test]
async fn authenticate_with_origin_resolves_origin_per_token_kind() {
    let svc = service();
    let user = svc.create_user("tina", Role::Admin).await.unwrap();
    let (access, _refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = access.row.family_id.clone().unwrap();

    let (_principal, origin) = svc.authenticate_with_origin(&access.secret).await.unwrap();
    assert_eq!(origin.0, Some(family.clone()));

    let tainted_key = svc
        .issue_api_key_minted_by(&user.id, Some(family.clone()))
        .await
        .unwrap();
    let (_principal, origin) = svc
        .authenticate_with_origin(&tainted_key.secret)
        .await
        .unwrap();
    assert_eq!(origin.0, Some(family));

    let untainted_key = svc.issue_api_key(&user.id).await.unwrap();
    let (_principal, origin) = svc
        .authenticate_with_origin(&untainted_key.secret)
        .await
        .unwrap();
    assert_eq!(origin.0, None);
}

#[tokio::test]
async fn rotate_refresh_token_lost_race_burns_family_and_mints_nothing() {
    // Simulates two concurrent `rotate_refresh_token` calls for the same
    // refresh token: both would see `revoked_at: None` in their own
    // fetch, but only one wins the atomic `revoke_token` update. The
    // loser must observe `revoke_token` return `false` and treat it
    // exactly like reuse — burn the whole family and fail closed —
    // rather than silently proceeding to mint a second live refresh
    // +access pair into the same family.
    let svc = service();
    let user = svc.create_user("river", Role::Admin).await.unwrap();
    let refresh = svc.issue_refresh_token(&user.id).await.unwrap();

    svc.store.poison_next_revoke(&refresh.row.id).await;

    let err = svc.rotate_refresh_token(&refresh.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    // The family-revoke fallback must still have caught the original
    // token (whose `revoked_at` the poisoned call itself left alone).
    let stored = svc
        .store
        .find_token_by_hash(&refresh.row.secret_hash)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stored.revoked_at.is_some(),
        "the family revoke must catch the token even though the \
             poisoned revoke_token call didn't"
    );

    // No new refresh/access pair was minted for the loser.
    let tokens = svc.store.list_tokens_for_user(&user.id).await.unwrap();
    assert_eq!(
        tokens.len(),
        1,
        "a lost-race rotation must not mint anything"
    );
    assert!(tokens[0].revoked_at.is_some());
}

#[tokio::test]
async fn authenticate_rejects_refresh_token_as_bearer_credential() {
    let svc = service();
    let user = svc.create_user("sasha", Role::Admin).await.unwrap();
    let refresh = svc.issue_refresh_token(&user.id).await.unwrap();

    let err = svc.authenticate(&refresh.secret).await.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn authenticate_still_accepts_access_token_and_api_key() {
    let svc = service();
    let user = svc.create_user("tara", Role::Admin).await.unwrap();
    let access = svc.issue_access_token(&user.id).await.unwrap();
    let api_key = svc.issue_api_key(&user.id).await.unwrap();

    assert_eq!(
        svc.authenticate(&access.secret).await.unwrap().user_id,
        user.id
    );
    assert_eq!(
        svc.authenticate(&api_key.secret).await.unwrap().user_id,
        user.id
    );
}

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
            None,
        )
        .await
        .unwrap();

    let (redeemed, origin) = svc
        .redeem_auth_code(
            &issued.secret,
            "localdb-cli",
            "http://127.0.0.1:1234/cb",
            &verifier,
        )
        .await
        .unwrap();
    assert_eq!(redeemed.id, user.id);
    assert_eq!(origin, None, "a code issued with no ancestry redeems none");
}

#[tokio::test]
async fn redeem_auth_code_wrong_verifier_fails() {
    let svc = service();
    let user = svc.create_user("jack", Role::Admin).await.unwrap();
    let (_verifier, challenge) = crate::auth::generate_pkce_pair();
    let issued = svc
        .issue_auth_code(
            "localdb-cli",
            &user.id,
            "http://127.0.0.1:1/cb",
            &challenge,
            None,
        )
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
        .issue_auth_code(
            "localdb-cli",
            &user.id,
            "http://127.0.0.1:1/cb",
            &challenge,
            None,
        )
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
        minted_by_family: None,
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
        .issue_auth_code(
            "localdb-cli",
            &user.id,
            "http://127.0.0.1:1/cb",
            &challenge,
            None,
        )
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
        .issue_auth_code(
            "localdb-cli",
            &user.id,
            "http://127.0.0.1:1/cb",
            &challenge,
            None,
        )
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

#[tokio::test]
async fn is_known_client_recognizes_builtin_and_registered() {
    let svc = service();
    assert!(svc.is_known_client(LOCALDB_CLI_CLIENT_ID).await.unwrap());
    assert!(!svc.is_known_client("nonexistent").await.unwrap());

    let registered = svc
        .register_client(
            vec!["http://127.0.0.1:4000/cb".to_string()],
            Some("Test Client".to_string()),
            None,
            None,
            None,
            &test_registration_policy(),
        )
        .await
        .unwrap();
    assert!(svc.is_known_client(&registered.id).await.unwrap());
}

#[tokio::test]
async fn register_client_rejects_empty_redirect_uris() {
    let svc = service();
    let err = svc
        .register_client(vec![], None, None, None, None, &test_registration_policy())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RegisterClientError::Validation(RegistrationError::EmptyRedirectUris)
    ));
}

#[tokio::test]
async fn register_client_rejects_invalid_redirect_uri() {
    let svc = service();
    let err = svc
        .register_client(
            vec!["http://evil.com/cb".to_string()],
            None,
            None,
            None,
            None,
            &test_registration_policy(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        RegisterClientError::Validation(RegistrationError::InvalidRedirectUri(_))
    ));
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
            None,
            None,
            None,
            &test_registration_policy(),
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
        .register_client(
            vec!["https://app.example.com/cb".to_string()],
            None,
            None,
            None,
            None,
            &test_registration_policy(),
        )
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

#[tokio::test]
async fn failed_rotation_keeps_original_refresh_usable() {
    let svc = service();
    let user = svc.create_user("rotation", Role::Member).await.unwrap();
    let refresh = svc.issue_refresh_token(&user.id).await.unwrap();
    svc.store.poison_next_insert_token().await;
    assert!(svc.rotate_refresh_token(&refresh.secret).await.is_err());
    assert_eq!(
        svc.store
            .list_tokens_for_user(&user.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(svc.rotate_refresh_token(&refresh.secret).await.is_ok());
}

// -----------------------------------------------------------------
// Compromise closure: a session created by authorizing with another
// session's credential, and every key minted under it, is reachable from
// reuse detection on the original session — and a marked family refuses to
// mint anything new, even racing the sweep that marked it.
// -----------------------------------------------------------------

#[tokio::test]
async fn compromised_family_refuses_a_new_key_mint_after_the_sweep() {
    let svc = service();
    let user = svc.create_user("uma", Role::Admin).await.unwrap();
    let (access, _refresh) = svc.issue_session(&user.id, None).await.unwrap();
    let family = access.row.family_id.clone().unwrap();

    svc.store.revoke_compromised_family(&family).await.unwrap();

    let err = svc
        .issue_api_key_minted_by(&user.id, Some(family))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    assert_eq!(
        svc.store
            .list_tokens_for_user(&user.id)
            .await
            .unwrap()
            .len(),
        2,
        "the refused mint must not have inserted anything"
    );
}

/// A session (B) created by authorizing with session A's own access token
/// as the `/authorize` credential — the service-level shape of
/// `server::auth::oauth::handle_auth_code_grant` — carries A's family as
/// its ancestry. Reuse detection on A must reach B's access and refresh
/// tokens, and any key minted under B, not just A's own tokens.
#[tokio::test]
async fn reuse_detection_on_a_revokes_a_descendant_session_and_its_keys() {
    let svc = service();
    let user = svc.create_user("victor", Role::Admin).await.unwrap();
    let (access_a, refresh_a) = svc.issue_session(&user.id, None).await.unwrap();
    let family_a = access_a.row.family_id.clone().unwrap();

    // Authorize with A's access token: the auth code inherits A's family as
    // its own ancestry, exactly as `authenticate_with_origin` would resolve
    // it for that credential.
    let (verifier, challenge) = crate::auth::generate_pkce_pair();
    let issued_code = svc
        .issue_auth_code(
            "localdb-cli",
            &user.id,
            "http://127.0.0.1:1/cb",
            &challenge,
            Some(family_a.clone()),
        )
        .await
        .unwrap();
    let (_redeemed_user, origin) = svc
        .redeem_auth_code(
            &issued_code.secret,
            "localdb-cli",
            "http://127.0.0.1:1/cb",
            &verifier,
        )
        .await
        .unwrap();
    assert_eq!(origin, Some(family_a.clone()));

    let (access_b, refresh_b) = svc.issue_session(&user.id, origin).await.unwrap();
    let family_b = access_b.row.family_id.clone().unwrap();
    assert_ne!(family_a, family_b, "B is its own, distinct session");

    let key_under_b = svc
        .issue_api_key_minted_by(&user.id, Some(family_b))
        .await
        .unwrap();

    // Reuse detection on A's own refresh token.
    let (rotated_access_a, rotated_refresh_a) =
        svc.rotate_refresh_token(&refresh_a.secret).await.unwrap();
    let err = svc
        .rotate_refresh_token(&refresh_a.secret)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    assert!(
        svc.authenticate(&rotated_access_a.secret).await.is_err(),
        "A's own rotated access token must die too"
    );
    assert!(svc.authenticate(&access_b.secret).await.is_err());
    assert!(svc.rotate_refresh_token(&refresh_b.secret).await.is_err());
    assert!(svc.authenticate(&key_under_b.secret).await.is_err());
    assert!(svc
        .rotate_refresh_token(&rotated_refresh_a.secret)
        .await
        .is_err());
}

#[tokio::test]
async fn logout_of_a_leaves_b_and_its_keys_working_and_later_mints_under_a_still_succeed() {
    let svc = service();
    let user = svc.create_user("wendy", Role::Admin).await.unwrap();
    let (_access_a, refresh_a) = svc.issue_session(&user.id, None).await.unwrap();
    let family_a = refresh_a.row.family_id.clone().unwrap();

    let (access_b, refresh_b) = svc
        .issue_session(&user.id, Some(family_a.clone()))
        .await
        .unwrap();
    let key_under_b = svc
        .issue_api_key_minted_by(&user.id, access_b.row.family_id.clone())
        .await
        .unwrap();

    // Logout (RFC 7009) ends only A's own session.
    assert!(svc.revoke_by_secret(&refresh_a.secret).await.unwrap());

    assert!(svc.authenticate(&access_b.secret).await.is_ok());
    assert!(svc.authenticate(&key_under_b.secret).await.is_ok());
    assert!(svc.rotate_refresh_token(&refresh_b.secret).await.is_ok());

    // A's family was never marked compromised by an ordinary logout, so a
    // later mint under it still succeeds.
    let later_key = svc
        .issue_api_key_minted_by(&user.id, Some(family_a))
        .await
        .unwrap();
    assert!(svc.authenticate(&later_key.secret).await.is_ok());
}

#[tokio::test]
async fn a_rotated_descendant_token_still_carries_the_ancestor_family() {
    let svc = service();
    let user = svc.create_user("xena", Role::Admin).await.unwrap();
    let (_access_a, refresh_a) = svc.issue_session(&user.id, None).await.unwrap();
    let family_a = refresh_a.row.family_id.clone().unwrap();

    let (_access_b, refresh_b) = svc
        .issue_session(&user.id, Some(family_a.clone()))
        .await
        .unwrap();

    let (rotated_access_b, rotated_refresh_b) =
        svc.rotate_refresh_token(&refresh_b.secret).await.unwrap();
    assert_eq!(
        rotated_access_b.row.minted_by_family,
        Some(family_a.clone())
    );
    assert_eq!(rotated_refresh_b.row.minted_by_family, Some(family_a));
}

// -----------------------------------------------------------------
// `register_client`'s registration-cap policy (specs/05-surfaces.md §3.1):
// the store-layer behavior itself is covered directly against
// `FakeAuthStore`/`store-libsql` (`prune_and_create_oauth_client`); these
// two just prove `AuthService::register_client` wires its `policy`
// argument through and maps the outcome correctly.
// -----------------------------------------------------------------

#[tokio::test]
async fn register_client_evicts_the_oldest_unused_client_once_the_cap_is_hit() {
    let svc = service();
    let policy = crate::auth::client::RegistrationPolicy {
        max_dynamic_clients: 1,
        unused_client_ttl_secs: 86400,
    };
    let first = svc
        .register_client(
            vec!["https://app.example.com/cb".to_string()],
            None,
            None,
            None,
            None,
            &policy,
        )
        .await
        .expect("first registration should succeed under the cap");

    // The first client never completed an authorization, so the second
    // registration evicts it to make room rather than being refused.
    let second = svc
        .register_client(
            vec!["https://app2.example.com/cb".to_string()],
            None,
            None,
            None,
            None,
            &policy,
        )
        .await
        .expect("registering past the cap should evict the unused client, not refuse");
    assert!(!svc.is_known_client(&first.id).await.unwrap());
    assert!(svc.is_known_client(&second.id).await.unwrap());
}

#[tokio::test]
async fn register_client_errors_cap_reached_once_every_slot_is_used() {
    let svc = service();
    let user = svc.create_user("alice", Role::Member).await.unwrap();
    let policy = crate::auth::client::RegistrationPolicy {
        max_dynamic_clients: 1,
        unused_client_ttl_secs: 86400,
    };
    let first = svc
        .register_client(
            vec!["https://app.example.com/cb".to_string()],
            None,
            None,
            None,
            None,
            &policy,
        )
        .await
        .expect("first registration should succeed under the cap");
    // Give the first client a completed authorization — it is now "used"
    // and must never be evicted, even though it is the only dynamic client
    // and the cap is 1.
    svc.issue_auth_code(
        &first.id,
        &user.id,
        "https://app.example.com/cb",
        "challenge",
        None,
    )
    .await
    .unwrap();

    let err = svc
        .register_client(
            vec!["https://app2.example.com/cb".to_string()],
            None,
            None,
            None,
            None,
            &policy,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, RegisterClientError::CapReached));
    assert!(svc.is_known_client(&first.id).await.unwrap());
}

#[tokio::test]
async fn register_client_with_the_unlimited_policy_never_prunes_or_caps() {
    let svc = service();
    let policy = crate::auth::client::RegistrationPolicy::unlimited();

    let old_client = svc
        .register_client(
            vec!["https://app0.example.com/cb".to_string()],
            None,
            None,
            None,
            None,
            &policy,
        )
        .await
        .expect("registration should succeed");

    // Tiny-cap-worth of further registrations must all succeed too — the
    // unlimited policy has no cap to hit.
    for i in 1..5 {
        svc.register_client(
            vec![format!("https://app{i}.example.com/cb")],
            None,
            None,
            None,
            None,
            &policy,
        )
        .await
        .expect("the unlimited policy must never refuse a registration");
    }

    // The first client is old and was never used, but the unlimited policy
    // prunes and evicts nothing.
    assert!(svc.is_known_client(&old_client.id).await.unwrap());
}

#[tokio::test]
async fn register_client_cap_does_not_count_the_builtin_client() {
    let svc = service();
    // A cap of 1 is reachable by exactly one dynamic registration — the
    // built-in `localdb-cli` client (seeded at the store layer in
    // production, absent from `FakeAuthStore` by construction) must never
    // count toward it either way.
    let policy = crate::auth::client::RegistrationPolicy {
        max_dynamic_clients: 1,
        unused_client_ttl_secs: 86400,
    };
    svc.register_client(
        vec!["https://app.example.com/cb".to_string()],
        None,
        None,
        None,
        None,
        &policy,
    )
    .await
    .expect("registering the only dynamic client under the cap should succeed");
}
