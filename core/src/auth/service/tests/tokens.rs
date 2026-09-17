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
