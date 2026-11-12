//! OAuth2 authorization code (RFC 6749 §4.1 + PKCE) cases shared by every
//! `AuthStore` implementation.

use std::sync::Arc;

use crate::auth::{
    generate_pkce_pair, mint_secret, rfc3339_from_now, AuthCodeRow, AuthService, AuthStore, Role,
    LOCALDB_CLI_CLIENT_ID,
};
use crate::ids::new_ulid;
use crate::Error;

const REDIRECT_URI: &str = "http://127.0.0.1:1/cb";

pub async fn issue_and_redeem_once<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = svc
        .create_user("auth-codes-redeem", Role::Admin)
        .await
        .unwrap();
    let (verifier, challenge) = generate_pkce_pair();
    let issued = svc
        .issue_auth_code(
            LOCALDB_CLI_CLIENT_ID,
            &user.id,
            REDIRECT_URI,
            &challenge,
            None,
        )
        .await
        .unwrap();

    let (redeemed, origin) = svc
        .redeem_auth_code(
            &issued.secret,
            LOCALDB_CLI_CLIENT_ID,
            REDIRECT_URI,
            &verifier,
        )
        .await
        .unwrap();
    assert_eq!(redeemed.id, user.id);
    assert_eq!(origin, None);
}

pub async fn second_redemption_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = svc
        .create_user("auth-codes-single-use", Role::Admin)
        .await
        .unwrap();
    let (verifier, challenge) = generate_pkce_pair();
    let issued = svc
        .issue_auth_code(
            LOCALDB_CLI_CLIENT_ID,
            &user.id,
            REDIRECT_URI,
            &challenge,
            None,
        )
        .await
        .unwrap();
    svc.redeem_auth_code(
        &issued.secret,
        LOCALDB_CLI_CLIENT_ID,
        REDIRECT_URI,
        &verifier,
    )
    .await
    .unwrap();

    let err = svc
        .redeem_auth_code(
            &issued.secret,
            LOCALDB_CLI_CLIENT_ID,
            REDIRECT_URI,
            &verifier,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}

pub async fn wrong_pkce_verifier_refused_without_consuming<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = svc
        .create_user("auth-codes-wrong-verifier", Role::Admin)
        .await
        .unwrap();
    let (verifier, challenge) = generate_pkce_pair();
    let issued = svc
        .issue_auth_code(
            LOCALDB_CLI_CLIENT_ID,
            &user.id,
            REDIRECT_URI,
            &challenge,
            None,
        )
        .await
        .unwrap();

    let err = svc
        .redeem_auth_code(
            &issued.secret,
            LOCALDB_CLI_CLIENT_ID,
            REDIRECT_URI,
            "wrong-verifier",
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    // Not consumed: the correct verifier still redeems it.
    svc.redeem_auth_code(
        &issued.secret,
        LOCALDB_CLI_CLIENT_ID,
        REDIRECT_URI,
        &verifier,
    )
    .await
    .unwrap();
}

pub async fn wrong_redirect_uri_refused_without_consuming<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = svc
        .create_user("auth-codes-wrong-redirect", Role::Admin)
        .await
        .unwrap();
    let (verifier, challenge) = generate_pkce_pair();
    let issued = svc
        .issue_auth_code(
            LOCALDB_CLI_CLIENT_ID,
            &user.id,
            REDIRECT_URI,
            &challenge,
            None,
        )
        .await
        .unwrap();

    let err = svc
        .redeem_auth_code(
            &issued.secret,
            LOCALDB_CLI_CLIENT_ID,
            "http://127.0.0.1:9999/cb",
            &verifier,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    svc.redeem_auth_code(
        &issued.secret,
        LOCALDB_CLI_CLIENT_ID,
        REDIRECT_URI,
        &verifier,
    )
    .await
    .unwrap();
}

pub async fn wrong_client_refused_without_consuming<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let user = svc
        .create_user("auth-codes-wrong-client", Role::Admin)
        .await
        .unwrap();
    let (verifier, challenge) = generate_pkce_pair();
    let issued = svc
        .issue_auth_code(
            LOCALDB_CLI_CLIENT_ID,
            &user.id,
            REDIRECT_URI,
            &challenge,
            None,
        )
        .await
        .unwrap();

    let err = svc
        .redeem_auth_code(&issued.secret, "some-other-client", REDIRECT_URI, &verifier)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));

    svc.redeem_auth_code(
        &issued.secret,
        LOCALDB_CLI_CLIENT_ID,
        REDIRECT_URI,
        &verifier,
    )
    .await
    .unwrap();
}

/// Built directly through the store (not `issue_auth_code`, which always
/// mints a far-future expiry) with an explicit past `expires_at` — avoids
/// racing the wall clock for the boundary itself.
pub async fn expired_code_refused<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store.clone());
    let user = svc
        .create_user("auth-codes-expired", Role::Admin)
        .await
        .unwrap();
    let (verifier, challenge) = generate_pkce_pair();
    let minted = mint_secret();
    let row = AuthCodeRow {
        id: new_ulid(),
        client_id: LOCALDB_CLI_CLIENT_ID.to_string(),
        user_id: user.id.clone(),
        code_hash: minted.hash.clone(),
        code_challenge: challenge,
        code_challenge_method: "S256".to_string(),
        redirect_uri: REDIRECT_URI.to_string(),
        expires_at: rfc3339_from_now(-3600),
        consumed_at: None,
        created_at: rfc3339_from_now(-3600),
        minted_by_family: None,
    };
    store.create_auth_code(&row).await.unwrap();

    let err = svc
        .redeem_auth_code(
            &minted.secret,
            LOCALDB_CLI_CLIENT_ID,
            REDIRECT_URI,
            &verifier,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
}
