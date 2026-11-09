use super::*;

use mcp::proxy::BearerSource as _;
use tempfile::TempDir;

fn ctx_with_config(config_file: &std::path::Path) -> CliContext {
    CliContext {
        config: Some(config_file.to_path_buf()),
        json: false,
        stores: vec![],
        yes: false,
        daemon_url: None,
        config_env: None,
        api_key: None,
    }
}

/// A mock `/token` endpoint answering every `grant_type=refresh_token` POST
/// with a fixed rotated pair, regardless of which refresh token was sent —
/// enough to exercise `CliBearerSource` without reproducing
/// `daemon_client`'s own refresh-grant mock.
async fn start_mock_token_endpoint() -> (u16, tokio::task::JoinHandle<()>) {
    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(|| async {
            axum::Json(serde_json::json!({
                "access_token": "new_access",
                "refresh_token": "new_refresh",
                "expires_in": 3600
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (port, server)
}

#[tokio::test]
async fn current_refreshes_an_expired_cached_access_token() {
    let (port, server) = start_mock_token_endpoint().await;
    let base_url = format!("http://127.0.0.1:{port}");

    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("old_access".to_string()),
            refresh_token: Some("old_refresh".to_string()),
            access_expires_at: Some(localdb_core::auth::rfc3339_from_now(-10)),
        },
    )
    .unwrap();

    let source = CliBearerSource {
        ctx: ctx_with_config(&config_file),
        base_url: base_url.clone(),
    };

    assert_eq!(source.current().await.as_deref(), Some("new_access"));
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(
        entry.access_token.as_deref(),
        Some("new_access"),
        "the rotated pair must be persisted back to credentials.json"
    );

    server.abort();
}

#[tokio::test]
async fn refresh_forces_a_new_token_even_when_the_cached_one_has_not_expired() {
    let (port, server) = start_mock_token_endpoint().await;
    let base_url = format!("http://127.0.0.1:{port}");

    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("rejected_access".to_string()),
            refresh_token: Some("old_refresh".to_string()),
            // Unexpired: only `refresh`'s explicit `rejected` match should
            // trigger a refresh here, not the proactive expiry check.
            access_expires_at: Some(localdb_core::auth::rfc3339_from_now(3600)),
        },
    )
    .unwrap();

    let source = CliBearerSource {
        ctx: ctx_with_config(&config_file),
        base_url,
    };

    assert_eq!(
        source.refresh("rejected_access").await.as_deref(),
        Some("new_access")
    );

    server.abort();
}

#[tokio::test]
async fn resolved_key_matches_a_shorter_stored_prefix_of_the_daemon_base_url() {
    // The daemon base URL `CliBearerSource` is given need not be the exact
    // string `localdb login` stored a credential under — only a
    // path-segment prefix of it (same rule `try_refresh_and_persist`
    // applies to a request URL). `resolved_key` must apply that same
    // longest-prefix match rather than requiring `base_url` to equal the
    // stored key verbatim.
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        "https://example.com/team",
        crate::credentials::CredentialEntry {
            secret: Some("ldb_team".to_string()),
            access_token: None,
            refresh_token: None,
            access_expires_at: None,
        },
    )
    .unwrap();

    let source = CliBearerSource {
        ctx: ctx_with_config(&config_file),
        base_url: "https://example.com/team/mcp-route".to_string(),
    };

    assert_eq!(source.resolved_key(), "https://example.com/team");
    assert_eq!(source.current().await.as_deref(), Some("ldb_team"));
}
