//! OAuth2 dynamic client registration (RFC 7591) cases shared by every
//! `AuthStore` implementation.

use std::sync::Arc;

use crate::auth::{AuthService, AuthStore, LOCALDB_CLI_CLIENT_ID};

pub async fn register_and_lookup<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    let registered = svc
        .register_client(
            vec!["https://app.example.com/cb".to_string()],
            Some("Contract Test Client".to_string()),
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert!(svc.is_known_client(&registered.id).await.unwrap());
    assert!(svc
        .validate_client_redirect_uri(&registered.id, "https://app.example.com/cb")
        .await
        .unwrap());
    // No loopback-any-port exception for a registered client — exact match
    // only (specs/05-surfaces.md §3.1).
    assert!(!svc
        .validate_client_redirect_uri(&registered.id, "https://app.example.com/other")
        .await
        .unwrap());
}

pub async fn unknown_client_is_not_known<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    assert!(!svc.is_known_client("clients-unknown-id").await.unwrap());
    assert!(!svc
        .validate_client_redirect_uri("clients-unknown-id", "https://app.example.com/cb")
        .await
        .unwrap());
}

/// The built-in `localdb-cli` client is always known and keeps its RFC
/// 8252 §7.3 loopback-any-port exception — no registration needed.
pub async fn builtin_client_known_with_loopback_any_port<S: AuthStore>(store: Arc<S>) {
    let svc = AuthService::new(store);
    assert!(svc.is_known_client(LOCALDB_CLI_CLIENT_ID).await.unwrap());
    assert!(svc
        .validate_client_redirect_uri(LOCALDB_CLI_CLIENT_ID, "http://127.0.0.1:1/cb")
        .await
        .unwrap());
    assert!(svc
        .validate_client_redirect_uri(LOCALDB_CLI_CLIENT_ID, "http://127.0.0.1:65535/cb")
        .await
        .unwrap());
}
