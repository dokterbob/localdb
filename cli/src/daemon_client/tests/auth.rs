use super::super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tempfile::TempDir;

/// `fresh_bearer_for_url` matches the stored key against the full request
/// URL by string prefix, with no URL reparsing on either side — a
/// credential keyed by a bracketed IPv6 base URL (as `login` writes it,
/// from `daemon.url`'s `SocketAddr` display) is found for a request built
/// from that same base URL.
#[tokio::test]
async fn fresh_bearer_for_url_matches_credential_written_for_bracketed_ipv6_base_url() {
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
        fresh_bearer_for_url(&ctx, &request_url).await.as_deref(),
        Some("ldb_ipv6_access"),
        "the credential written for the bracketed IPv6 base URL must be \
             found again when looked up via the same base URL derivation"
    );
}

// -----------------------------------------------------------------
// 401-retry-with-refresh
// -----------------------------------------------------------------

/// A minimal stateful mock daemon (hand-rolled raw TCP, mirroring
/// `localdb/tests/auth_cli.rs`'s style): any route under `{token_prefix}/token`
/// answers a refresh grant; every other route answers 401 unless
/// `Authorization: Bearer new_access` is presented. `token_prefix` is empty
/// for a bare-origin daemon, or a path like `/team` for a path-prefixed one.
fn start_refresh_mock_daemon() -> u16 {
    start_refresh_mock_daemon_with_prefix("").0
}

/// `POST {token_prefix}/token` with `grant_type=refresh_token&refresh_token=old_refresh`
/// answers a fresh `new_access`/`new_refresh` pair, anything else `invalid_grant`.
///
/// Returns the bound port plus `(token_calls, unauthorized_calls)` counters
/// so a proactive-refresh test can assert the stale/soon-to-expire token was
/// redeemed before ever being sent to the protected route, rather than only
/// after a 401.
fn start_refresh_mock_daemon_with_prefix(
    token_prefix: &str,
) -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();
    let token_path = format!("{token_prefix}/token");
    let token_calls = Arc::new(AtomicUsize::new(0));
    let unauthorized_calls = Arc::new(AtomicUsize::new(0));
    let token_calls_for_thread = token_calls.clone();
    let unauthorized_calls_for_thread = unauthorized_calls.clone();

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

            let response = if path.starts_with(&token_path) {
                token_calls_for_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                unauthorized_calls_for_thread.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    (port, token_calls, unauthorized_calls)
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

/// The fixture here is a token with no `access_expires_at` recorded at all,
/// so the proactive expiry check never fires — the retry happens only
/// because the mock daemon rejects `old_access` with a 401, exercising the
/// reactive backstop in `daemon_request_async`, not the proactive refresh
/// `fresh_bearer_for_url` performs up front.
#[tokio::test]
async fn rejected_access_token_is_retried_once_with_refreshed_token() {
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
        "rejected-token retry should succeed: {:?}",
        result.err()
    );

    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("new_access"));
    assert_eq!(entry.refresh_token.as_deref(), Some("new_refresh"));
}

/// A daemon served behind a path prefix: the credential is keyed by the
/// prefixed base URL (`.../team`), not its bare origin. The first request's
/// bearer lookup finds it by prefix match, the 401 retry's refresh grant
/// posts to `.../team/token`, and the rotated pair is written back under
/// that same `.../team` key.
#[tokio::test]
async fn path_prefixed_daemon_credential_is_found_and_refreshed_under_the_same_key() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let (port, _token_calls, _unauthorized_calls) = start_refresh_mock_daemon_with_prefix("/team");
    let base_url = format!("http://127.0.0.1:{port}/team");

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
        "the path-prefixed credential must be found and refreshed: {:?}",
        result.err()
    );

    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("new_access"));
    assert_eq!(entry.refresh_token.as_deref(), Some("new_refresh"));
}

// -----------------------------------------------------------------
// Proactive refresh through `daemon_request_async`: an expired, or
// soon-to-expire, cached access token must never be sent to the protected
// route at all — it is refreshed up front, so the mock's 401 counter stays
// at zero and the only `/token` call is the proactive one.
// -----------------------------------------------------------------

#[tokio::test]
async fn daemon_request_proactively_refreshes_an_already_expired_cached_token() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let (port, token_calls, unauthorized_calls) = start_refresh_mock_daemon_with_prefix("");
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
    let result = daemon_request_async(
        &ctx,
        reqwest::Method::GET,
        &format!("{base_url}/v1/status"),
        None,
    )
    .await;

    assert!(
        result.is_ok(),
        "the request after a proactive refresh should succeed: {:?}",
        result.err()
    );
    assert_eq!(
        token_calls.load(Ordering::SeqCst),
        1,
        "the mock must see exactly one POST /token, issued before the API call"
    );
    assert_eq!(
        unauthorized_calls.load(Ordering::SeqCst),
        0,
        "an already-expired token must never reach the protected route in the first place"
    );
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("new_access"));
}

#[tokio::test]
async fn daemon_request_proactively_refreshes_a_token_expiring_within_the_margin() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let (port, token_calls, unauthorized_calls) = start_refresh_mock_daemon_with_prefix("");
    let base_url = format!("http://127.0.0.1:{port}");

    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: None,
            access_token: Some("old_access".to_string()),
            refresh_token: Some("old_refresh".to_string()),
            // Not yet expired, but inside the 30s refresh margin.
            access_expires_at: Some(localdb_core::auth::rfc3339_from_now(10)),
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
        "the request after a proactive refresh should succeed: {:?}",
        result.err()
    );
    assert_eq!(token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        unauthorized_calls.load(Ordering::SeqCst),
        0,
        "a token expiring within the refresh margin must be refreshed before use, not after a 401"
    );
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("new_access"));
}

/// Same proactive-refresh guarantee as the bare-origin cases above, but for
/// a credential keyed by a path-prefixed daemon base URL — the same bug
/// `path_prefixed_daemon_credential_is_found_and_refreshed_under_the_same_key`
/// guards for the reactive-401 path.
#[tokio::test]
async fn path_prefixed_daemon_credential_gets_the_same_proactive_refresh() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let (port, _token_calls, unauthorized_calls) = start_refresh_mock_daemon_with_prefix("/team");
    let base_url = format!("http://127.0.0.1:{port}/team");

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
    let result = daemon_request_async(
        &ctx,
        reqwest::Method::GET,
        &format!("{base_url}/v1/status"),
        None,
    )
    .await;

    assert!(
        result.is_ok(),
        "the path-prefixed credential's proactive refresh should succeed: {:?}",
        result.err()
    );
    assert_eq!(
        unauthorized_calls.load(Ordering::SeqCst),
        0,
        "the path-prefixed credential's expired token must be refreshed before use, not after a 401"
    );
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(entry.access_token.as_deref(), Some("new_access"));
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
// fresh_bearer_for_url: the shared proactive-refresh helper every
// daemon-bound CLI request goes through, called directly here with an
// already-resolved base URL (`daemon_request_async`'s own tests above
// exercise it indirectly, through a full request).
// -----------------------------------------------------------------

#[tokio::test]
async fn fresh_bearer_for_url_refreshes_an_expired_access_token() {
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
    let bearer = fresh_bearer_for_url(&ctx, &base_url).await;

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
async fn fresh_bearer_for_url_returns_unexpired_token_without_refreshing() {
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
    let bearer = fresh_bearer_for_url(&ctx, &base_url).await;

    assert_eq!(bearer.as_deref(), Some("still_good"));
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url).unwrap();
    assert_eq!(
        entry.refresh_token.as_deref(),
        Some("unused_refresh"),
        "the refresh token must be untouched — no refresh attempt should happen"
    );
}

#[tokio::test]
async fn fresh_bearer_for_url_env_override_wins_without_touching_credentials() {
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

    let bearer = fresh_bearer_for_url(&ctx, &base_url).await;
    assert_eq!(bearer.as_deref(), Some("ldb_env_key"));

    let credentials_file = crate::credentials::credentials_path(&config_file);
    assert!(
        !credentials_file.exists(),
        "the env override must be returned without ever creating/touching credentials.json"
    );
}

#[tokio::test]
async fn fresh_bearer_for_url_empty_env_key_falls_back_to_credentials_file() {
    let dir = TempDir::new().unwrap();
    let config_file = dir.path().join("config.yaml");
    std::fs::write(&config_file, "version: 1\n").unwrap();
    let base_url = "http://127.0.0.1:1".to_string();

    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::write_entry(
        &credentials_file,
        &base_url,
        crate::credentials::CredentialEntry {
            secret: Some("ldb_cached".to_string()),
            access_token: None,
            refresh_token: None,
            access_expires_at: None,
        },
    )
    .unwrap();

    let mut ctx = ctx_with_config(&config_file);
    ctx.api_key = Some(String::new());

    assert_eq!(
        fresh_bearer_for_url(&ctx, &base_url).await.as_deref(),
        Some("ldb_cached"),
        "an empty LOCALDB_API_KEY must be treated as unset, not as a literal empty bearer"
    );
}

#[tokio::test]
async fn fresh_bearer_for_url_falls_back_to_stale_token_when_refresh_fails() {
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
    let bearer = fresh_bearer_for_url(&ctx, &base_url).await;

    assert_eq!(
        bearer.as_deref(),
        Some("old_access"),
        "a failed refresh should still hand back the stale cached token \
             (best effort) rather than nothing at all"
    );
}

// Invoked in separate OS processes by the test below; ordinary test runs do nothing.
//
// This worker calls straight into the refresh path with no rendezvous of
// its own. The rendezvous that makes the two processes' *first*, unlocked
// reads of the stale credential genuinely concurrent lives inside
// `refresh_cached_bearer` itself (`daemon_client.rs`'s
// `rendezvous_before_credential_lock`, gated on
// `LOCALDB_TEST_REFRESH_READY_DIR`): each process writes its own marker and
// blocks for its sibling's right after reading the stale entry and before
// acquiring the credential lock. Rendezvousing there, rather than at the
// top of this function, is what proves `CredentialLock` itself serializes
// the race — both processes are provably inside the read-then-lock window
// at once, so one process finishing the whole refresh (lock, redeem,
// persist) before the other even starts would fail the test, not just
// happen to pass it.
#[test]
fn refresh_process_worker() {
    let Ok(config) = std::env::var("LOCALDB_TEST_REFRESH_CONFIG") else {
        return;
    };
    let base = std::env::var("LOCALDB_TEST_REFRESH_URL").unwrap();

    let ctx = ctx_with_config(std::path::Path::new(&config));
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        if std::env::var("LOCALDB_TEST_REFRESH_PROACTIVE").is_ok() {
            assert_eq!(
                fresh_bearer_for_url(&ctx, &base).await.as_deref(),
                Some("new_access")
            );
        } else {
            daemon_request_async(
                &ctx,
                reqwest::Method::GET,
                &format!("{base}/v1/status"),
                None,
            )
            .await
            .unwrap();
        }
    });
}

/// Two processes independently discover the same stale refresh token and
/// call into `refresh_cached_bearer` at genuinely the same time (enforced by
/// that function's own `rendezvous_before_credential_lock` test hook, not by
/// anything here). What this proves: the credential lock
/// (`cli/src/daemon_client.rs`'s use of `CredentialLock` around the
/// re-check + `/token` call + persist) correctly serializes that race —
/// exactly one process ever redeems the refresh token (`rotations == 1`),
/// the loser gets back the winner's freshly written access token without
/// ever calling `/token` itself, and an unrelated origin's cached entry
/// survives the read-modify-write untouched.
///
/// What this does *not* prove: that both processes are ever inside the
/// mock `/token` handler at the same instant. By design, the loser's
/// locked re-check of the credential (`daemon_client.rs`'s `needs_refresh`
/// call taken under the lock, after `CredentialLock::acquire` returns)
/// observes the winner's already-written fresh entry and returns early
/// without calling `/token` at all — so a correct implementation never
/// sends a second `/token` request in the first place. The mock's
/// `count == 0` branch exists only as a defensive check: if a regression
/// ever did let a second request through, it would hit `BAD_REQUEST` and
/// fail one of the two child processes instead of silently passing.
#[tokio::test]
async fn concurrent_processes_share_one_refresh_and_preserve_other_origins() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let rotations = Arc::new(AtomicUsize::new(0));
    let counter = rotations.clone();
    let app = axum::Router::new()
        .route("/token", axum::routing::post(move || {
            let counter = counter.clone();
            async move {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                let status = if count == 0 { axum::http::StatusCode::OK } else { axum::http::StatusCode::BAD_REQUEST };
                (status, axum::Json(serde_json::json!({"access_token":"new_access", "refresh_token":"new_refresh", "expires_in":3600})))
            }
        }))
        .route("/v1/status", axum::routing::get(|headers: axum::http::HeaderMap| async move {
            let status = if headers.get("authorization").and_then(|h| h.to_str().ok()) == Some("Bearer new_access") { axum::http::StatusCode::OK } else { axum::http::StatusCode::UNAUTHORIZED };
            (status, axum::Json(serde_json::json!({})))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("config.yaml");
    let path = crate::credentials::credentials_path(&config);
    let ready_dir = dir.path().join("ready");
    std::fs::create_dir(&ready_dir).unwrap();
    crate::credentials::write_entry(
        &path,
        &base,
        crate::credentials::CredentialEntry {
            access_token: Some("old_access".into()),
            refresh_token: Some("old_refresh".into()),
            access_expires_at: Some("2000-01-01T00:00:00Z".into()),
            ..Default::default()
        },
    )
    .unwrap();
    crate::credentials::write_entry(
        &path,
        "https://other.example",
        crate::credentials::CredentialEntry {
            secret: Some("keep".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut children = Vec::new();
    for (worker_id, proactive) in [false, true].into_iter().enumerate() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "daemon_client::tests::auth::refresh_process_worker",
                "--nocapture",
            ])
            .env("LOCALDB_TEST_REFRESH_CONFIG", &config)
            .env("LOCALDB_TEST_REFRESH_URL", &base)
            .env("LOCALDB_TEST_REFRESH_READY_DIR", &ready_dir)
            .env("LOCALDB_TEST_REFRESH_WORKER_ID", worker_id.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if proactive {
            command.env("LOCALDB_TEST_REFRESH_PROACTIVE", "1");
        }
        children.push(command.spawn().unwrap());
    }
    for child in children {
        let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(rotations.load(Ordering::SeqCst), 1);
    assert_eq!(
        crate::credentials::lookup_secret(&path, "https://other.example").as_deref(),
        Some("keep")
    );
    assert_eq!(
        fresh_bearer_for_url(&ctx_with_config(&config), &base)
            .await
            .as_deref(),
        Some("new_access")
    );
    server.abort();
}

#[tokio::test]
async fn valid_cached_credentials_do_not_require_a_writable_lock_file() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("config.yaml");
    let path = crate::credentials::credentials_path(&config);
    let base = "https://daemon.example";
    crate::credentials::write_entry(
        &path,
        base,
        crate::credentials::CredentialEntry {
            secret: Some("read-only-key".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // Deterministically prevent opening a lock file, even when tests run as root.
    std::fs::create_dir(dir.path().join("credentials.lock")).unwrap();
    assert_eq!(
        fresh_bearer_for_url(&ctx_with_config(&config), base)
            .await
            .as_deref(),
        Some("read-only-key")
    );
}

/// A token that is still valid, but due for a proactive refresh (inside the
/// refresh margin), must not be dropped just because the lock file can't be
/// opened — the same unwritable-lock fixture as
/// `valid_cached_credentials_do_not_require_a_writable_lock_file`, but with
/// a soon-expiring token instead of a non-expiring one.
#[tokio::test]
async fn soon_expiring_credentials_fall_back_to_the_cached_bearer_when_the_lock_file_is_unwritable()
{
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("config.yaml");
    let path = crate::credentials::credentials_path(&config);
    let base = "https://daemon.example";
    crate::credentials::write_entry(
        &path,
        base,
        crate::credentials::CredentialEntry {
            access_token: Some("soon-expiring".into()),
            access_expires_at: Some(localdb_core::auth::rfc3339_from_now(10)),
            refresh_token: Some("refresh-me".into()),
            ..Default::default()
        },
    )
    .unwrap();
    // Deterministically prevent opening a lock file, even when tests run as root.
    std::fs::create_dir(dir.path().join("credentials.lock")).unwrap();

    let ctx = ctx_with_config(&config);
    assert_eq!(
        fresh_bearer_for_url(&ctx, base).await.as_deref(),
        Some("soon-expiring"),
        "a still-valid token must not be dropped just because a proactive \
         refresh can't take the lock"
    );

    assert_eq!(
        refresh_cached_bearer(&ctx, base, Some("soon-expiring")).await,
        None,
        "a bearer the daemon already rejected must never be reused, lock or no lock"
    );
}
