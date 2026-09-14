use super::super::*;

use tempfile::TempDir;

#[test]
fn base_url_of_extracts_origin_with_port() {
    assert_eq!(
        base_url_of("http://127.0.0.1:7700/v1/stores").as_deref(),
        Some("http://127.0.0.1:7700")
    );
}

#[test]
fn base_url_of_preserves_bracketed_ipv6() {
    assert_eq!(
        base_url_of("http://[::1]:7700/v1/status").as_deref(),
        Some("http://[::1]:7700")
    );
}

#[test]
fn base_url_of_without_port() {
    assert_eq!(
        base_url_of("https://daemon.example.com/v1/search").as_deref(),
        Some("https://daemon.example.com")
    );
}

/// Regression test for a reviewer claim that an IPv6 daemon origin key
/// might be written one way (e.g. by `login`, from the raw base URL
/// `probe_daemon`/`daemon.url` hand out) and looked up another way (via
/// `base_url_of` on the constructed request URL), causing a bracket
/// mismatch and a spurious 401. Both sides in fact go through the same
/// canonical `scheme://[host]:port` shape — `daemon.url` is written from
/// `std::net::SocketAddr`'s `Display` impl, which already brackets IPv6
/// (`[::1]:7700`), and `base_url_of` reserializes via `url::Url`, which
/// also brackets IPv6 host_str. This test writes a credential keyed by
/// the raw bracketed base URL (as `login` would) and confirms
/// `bearer_for_request` on a URL built from that same base URL finds it.
#[test]
fn bearer_for_request_matches_credential_written_for_bracketed_ipv6_base_url() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();

    let base_url = "http://[::1]:7700";
    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("ldb_ipv6_access".to_string()),
            refresh_token: None,
            access_expires_at: None,
        },
    )
    .unwrap();

    let ctx = ctx_with_config(&config_file);
    let request_url = format!("{base_url}/v1/status");
    assert_eq!(
        bearer_for_request(&ctx, &request_url).as_deref(),
        Some("ldb_ipv6_access"),
        "the credential written for the bracketed IPv6 base URL must be \
             found again when looked up via the same base URL derivation"
    );
}

// -----------------------------------------------------------------
// T4: 401-retry-with-refresh
// -----------------------------------------------------------------

/// A minimal stateful mock daemon (hand-rolled raw TCP, mirroring
/// `localdb/tests/auth_cli.rs`'s style): any non-`/token` route answers
/// 401 unless `Authorization: Bearer new_access` is presented; `POST
/// /token` with `grant_type=refresh_token&refresh_token=old_refresh`
/// answers a fresh `new_access`/`new_refresh` pair, anything else
/// `invalid_grant`.
fn start_refresh_mock_daemon() -> u16 {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .to_string();

            let mut auth: Option<String> = None;
            let mut content_length: usize = 0;
            loop {
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if lower.starts_with("authorization:") {
                    auth = Some(line["authorization:".len()..].trim().to_string());
                }
                if let Some(rest) = lower.strip_prefix("content-length:") {
                    content_length = rest.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                let _ = reader.read_exact(&mut body);
            }
            let body = String::from_utf8_lossy(&body).to_string();

            let response = if path.starts_with("/token") {
                if body.contains("grant_type=refresh_token")
                    && body.contains("refresh_token=old_refresh")
                {
                    let json = r#"{"access_token":"new_access","refresh_token":"new_refresh","expires_in":3600,"token_type":"Bearer"}"#;
                    format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            json.len(),
                            json
                        )
                } else {
                    let json = r#"{"error":"invalid_grant"}"#;
                    format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            json.len(),
                            json
                        )
                }
            } else if auth.as_deref() == Some("Bearer new_access") {
                let json = r#"{"status":"ok"}"#;
                format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        json.len(),
                        json
                    )
            } else {
                let json = r#"{"code":"unauthorized","message":"missing or expired bearer token"}"#;
                format!(
                        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nWWW-Authenticate: Bearer\r\nContent-Length: {}\r\n\r\n{}",
                        json.len(),
                        json
                    )
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });

    port
}

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

#[tokio::test]
async fn expired_access_token_is_retried_once_with_refreshed_token() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let port = start_refresh_mock_daemon();
    let base_url = format!("http://127.0.0.1:{port}");

    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("old_access".to_string()),
            refresh_token: Some("old_refresh".to_string()),
            access_expires_at: None,
        },
    )
    .unwrap();

    let ctx = ctx_with_config(&config_file);
    let result = daemon_request_async(
        &ctx,
        reqwest::Method::GET,
        &format!("{base_url}/v1/status"),
        None,
    )
    .await;

    assert!(
        result.is_ok(),
        "expired-token retry should succeed: {:?}",
        result.err()
    );

    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("new_access"));
    assert_eq!(entry.refresh_token.as_deref(), Some("new_refresh"));
}

#[tokio::test]
async fn no_refresh_token_available_surfaces_unauthorized_with_login_guidance() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let port = start_refresh_mock_daemon();
    let base_url = format!("http://127.0.0.1:{port}");

    // No credentials.json entry at all: nothing to refresh.
    let ctx = ctx_with_config(&config_file);
    let result = daemon_request_async(
        &ctx,
        reqwest::Method::GET,
        &format!("{base_url}/v1/status"),
        None,
    )
    .await;

    let err = result.unwrap_err();
    assert!(matches!(err, Error::Unauthorized { .. }));
    assert!(
        err.to_string().contains("login") || format!("{err:?}").contains("login"),
        "guidance should point at `localdb login`: {err}"
    );
}

#[tokio::test]
async fn stale_refresh_token_that_the_daemon_rejects_surfaces_unauthorized() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let port = start_refresh_mock_daemon();
    let base_url = format!("http://127.0.0.1:{port}");

    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("old_access".to_string()),
            refresh_token: Some("no-longer-valid".to_string()),
            access_expires_at: None,
        },
    )
    .unwrap();

    let ctx = ctx_with_config(&config_file);
    let result = daemon_request_async(
        &ctx,
        reqwest::Method::GET,
        &format!("{base_url}/v1/status"),
        None,
    )
    .await;

    assert!(matches!(result.unwrap_err(), Error::Unauthorized { .. }));
}

#[tokio::test]
async fn env_api_key_skips_refresh_attempt_entirely() {
    // LOCALDB_API_KEY is a static bearer, not part of the token-pair
    // rotation model — a 401 with it set must not attempt a refresh
    // grant at all (there is nothing to refresh it with).
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let port = start_refresh_mock_daemon();
    let base_url = format!("http://127.0.0.1:{port}");

    let credentials_file = crate::credentials::credentials_path(&config_file);
    // Even with a valid refresh token cached, the env override must win
    // and no refresh attempt should be made.
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("old_access".to_string()),
            refresh_token: Some("old_refresh".to_string()),
            access_expires_at: None,
        },
    )
    .unwrap();

    let mut ctx = ctx_with_config(&config_file);
    ctx.api_key = Some("ldb_env_override_that_is_wrong".to_string());

    let result = daemon_request_async(
        &ctx,
        reqwest::Method::GET,
        &format!("{base_url}/v1/status"),
        None,
    )
    .await;

    assert!(matches!(result.unwrap_err(), Error::Unauthorized { .. }));
    // The cached refresh token must be untouched — no refresh attempt happened.
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("old_access"));
}

// -----------------------------------------------------------------
// ensure_fresh_bearer: proactive pre-connect refresh for the MCP
// daemon-proxy handshake (`cmds::surface::run_mcp_async`), which has no
// cheap way to retry mid-stream on a 401 the way `daemon_request_async`
// does.
// -----------------------------------------------------------------

#[tokio::test]
async fn ensure_fresh_bearer_refreshes_an_expired_access_token() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let port = start_refresh_mock_daemon();
    let base_url = format!("http://127.0.0.1:{port}");

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

    let ctx = ctx_with_config(&config_file);
    let bearer = ensure_fresh_bearer(&ctx, &base_url).await;

    assert_eq!(
        bearer.as_deref(),
        Some("new_access"),
        "an expired access token with a live refresh token must be redeemed proactively"
    );
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("new_access"));
    assert_eq!(
        entry.refresh_token.as_deref(),
        Some("new_refresh"),
        "the rotated refresh token must be persisted, not just the access token"
    );
}

#[tokio::test]
async fn ensure_fresh_bearer_returns_unexpired_token_without_refreshing() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    // No mock daemon needed: a fresh token must never trigger a network
    // call, so any unreachable base URL will do — if the code tried to
    // refresh, the test would still pass by accident (redeem failure
    // falls back to the current token), so we additionally assert the
    // credentials file is untouched to catch a spurious refresh attempt.
    let base_url = "http://127.0.0.1:1".to_string();

    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("still_good".to_string()),
            refresh_token: Some("unused_refresh".to_string()),
            access_expires_at: Some(localdb_core::auth::rfc3339_from_now(3600)),
        },
    )
    .unwrap();

    let ctx = ctx_with_config(&config_file);
    let bearer = ensure_fresh_bearer(&ctx, &base_url).await;

    assert_eq!(bearer.as_deref(), Some("still_good"));
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(
        entry.refresh_token.as_deref(),
        Some("unused_refresh"),
        "the refresh token must be untouched — no refresh attempt should happen"
    );
}

#[tokio::test]
async fn ensure_fresh_bearer_env_override_wins_without_touching_credentials() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let base_url = "http://127.0.0.1:1".to_string();

    // No credentials.json entry at all — if the env override is not
    // returned verbatim first, this would fall through to `None`
    // instead, or (if the code were buggy) attempt a file read.
    let ctx = {
        let mut ctx = ctx_with_config(&config_file);
        ctx.api_key = Some("ldb_env_key".to_string());
        ctx
    };

    let bearer = ensure_fresh_bearer(&ctx, &base_url).await;
    assert_eq!(bearer.as_deref(), Some("ldb_env_key"));

    let credentials_file = crate::credentials::credentials_path(&config_file);
    assert!(
        !credentials_file.exists(),
        "the env override must be returned without ever creating/touching credentials.json"
    );
}

#[tokio::test]
async fn ensure_fresh_bearer_falls_back_to_stale_token_when_refresh_fails() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let port = start_refresh_mock_daemon();
    let base_url = format!("http://127.0.0.1:{port}");

    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("old_access".to_string()),
            // The mock daemon only accepts `old_refresh`; this one is
            // rejected, exercising the best-effort fallback.
            refresh_token: Some("no-longer-valid".to_string()),
            access_expires_at: Some(localdb_core::auth::rfc3339_from_now(-10)),
        },
    )
    .unwrap();

    let ctx = ctx_with_config(&config_file);
    let bearer = ensure_fresh_bearer(&ctx, &base_url).await;

    assert_eq!(
        bearer.as_deref(),
        Some("old_access"),
        "a failed refresh should still hand back the stale cached token \
             (best effort) rather than nothing at all"
    );
}
