use super::*;
use localdb_core::auth::{AuthStore as _, Role};
use localdb_core::config::schema::{
    DefaultsConfig, EmbeddingPolicy, IndexingPolicyConfig, RawConfig,
};
use server::{AppState, AuthMode, JobQueue, UrlRefreshScheduler};
use std::sync::Arc;
use tempfile::TempDir;

/// Build a real, in-process enforced daemon router on an ephemeral TCP
/// port, returning its base URL and `AppState` (so tests can seed
/// users/setup codes through the same live database the router serves).
async fn spawn_test_daemon() -> (TempDir, AppState, String) {
    let dir = TempDir::new().unwrap();
    let mut yaml_config = RawConfig {
        version: 1,
        server: Default::default(),
        paths: Default::default(),
        defaults: DefaultsConfig {
            indexing: IndexingPolicyConfig {
                embedding: EmbeddingPolicy {
                    provider: "fake".to_string(),
                    model: "default".to_string(),
                },
                ..Default::default()
            },
        },
        providers: vec![],
        ..Default::default()
    };
    yaml_config.version = 1;
    let queue = JobQueue::new();
    let state = AppState::new(
        yaml_config,
        dir.path().to_path_buf(),
        dir.path().to_path_buf().join("models"),
        queue.clone(),
        UrlRefreshScheduler::new(queue),
        AuthMode::Enforced,
    )
    .await
    .unwrap();

    let router = server::build_router(
        state.clone(),
        std::sync::Arc::new(server::mcp_bridge::AppStateStoreProvider::new(
            state.clone(),
        )),
        Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    // Give the server a moment to start accepting connections.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    (dir, state, base_url)
}

fn ctx_for(dir: &TempDir) -> CliContext {
    CliContext {
        config: Some(dir.path().join("config.yaml")),
        json: false,
        stores: vec![],
        yes: false,
        daemon_url: None,
        config_env: None,
        api_key: None,
    }
}

/// A fake "browser": parses the query params off the authorize URL,
/// submits the consent form with `credential` over a real HTTP POST.
/// Because `reqwest` follows redirects by default, the daemon's 303
/// response lands on our own ephemeral loopback listener exactly as a
/// real browser's navigation would — no mocking of the OAuth flow
/// itself, just of the "open a browser window" step.
fn fake_browser_opener(credential: String) -> impl Fn(&str) -> bool {
    move |authorize_url: &str| {
        let authorize_url = authorize_url.to_string();
        let credential = credential.clone();
        tokio::spawn(async move {
            let parsed = reqwest::Url::parse(&authorize_url).unwrap();
            let mut pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
            pairs.push(("credential".to_string(), credential));
            let client = reqwest::Client::new();
            let base = format!(
                "{}://{}",
                parsed.scheme(),
                parsed
                    .host_str()
                    .map(|h| format!("{h}:{}", parsed.port().unwrap_or(80)))
                    .unwrap()
            );
            let _ = client
                .post(format!("{base}/authorize"))
                .form(&pairs)
                .send()
                .await;
        });
        true
    }
}

#[tokio::test]
async fn perform_login_happy_path_persists_credentials() {
    let (dir, state, base_url) = spawn_test_daemon().await;
    let user = state
        .auth()
        .create_user("alice", Role::Admin)
        .await
        .unwrap();
    let api_key = state.auth().issue_api_key(&user.id).await.unwrap().secret;
    let ctx = ctx_for(&dir);

    let opener = fake_browser_opener(api_key);
    let result = perform_login(
        &ctx,
        &base_url,
        None,
        false,
        opener,
        std::time::Duration::from_secs(10),
    )
    .await;
    assert!(result.is_ok(), "login should succeed: {:?}", result.err());

    let credentials_file = crate::credentials::credentials_path(&dir.path().join("config.yaml"));
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url)
        .expect("credentials.json must have an entry for the daemon's base URL");
    assert!(entry.access_token.unwrap().starts_with("ldb_"));
    assert!(entry.refresh_token.unwrap().starts_with("ldb_"));
    assert!(entry.access_expires_at.is_some());
}

#[tokio::test]
async fn perform_login_wrong_credential_fails() {
    let (dir, _state, base_url) = spawn_test_daemon().await;
    let ctx = ctx_for(&dir);

    let opener = fake_browser_opener("ldb_not-a-real-key".to_string());
    let result = perform_login(
        &ctx,
        &base_url,
        None,
        false,
        opener,
        std::time::Duration::from_millis(500),
    )
    .await;
    assert!(result.is_err(), "login with a bogus credential must fail");
}

#[tokio::test]
async fn logout_clears_cached_entry_and_revokes() {
    let (dir, state, base_url) = spawn_test_daemon().await;
    let user = state.auth().create_user("bob", Role::Admin).await.unwrap();
    let api_key = state.auth().issue_api_key(&user.id).await.unwrap().secret;
    let ctx = ctx_for(&dir);

    let opener = fake_browser_opener(api_key);
    // 45s, not the 10s the sibling happy-path test uses: this is a
    // liveness budget, not an assertion. The spawn-daemon + browser-login
    // round trip measures ~8s idle but ~15s on a loaded machine, so a 10s
    // budget turns CPU contention into a spurious "timed out waiting for
    // the browser to complete login" failure. Nothing here asserts on
    // elapsed time; raising the ceiling only stops the test giving up early.
    perform_login(
        &ctx,
        &base_url,
        None,
        false,
        opener,
        std::time::Duration::from_secs(45),
    )
    .await
    .unwrap();

    let credentials_file = crate::credentials::credentials_path(&dir.path().join("config.yaml"));
    assert!(crate::credentials::lookup_entry(&credentials_file, &base_url).is_some());

    run_logout_async(&ctx, Some(&base_url)).await;

    assert!(
        crate::credentials::lookup_entry(&credentials_file, &base_url).is_none(),
        "logout must clear the cached entry"
    );
}

#[tokio::test]
async fn no_browser_setup_code_bootstrap_via_stdin_paste_placeholder() {
    // The oob (`--no-browser`) path reads the pasted code from stdin,
    // which isn't practical to drive in an automated unit test without
    // reassigning process stdin. The listener-based flow above already
    // exercises the full HTTP round trip (authorize -> token exchange
    // -> credentials.json); this test instead exercises the oob URL
    // construction directly, which is the part specific to
    // `--no-browser` and doesn't require stdin.
    let url = build_authorize_url(
        "http://127.0.0.1:7700",
        localdb_core::auth::OOB_REDIRECT_URI,
        "state1",
        "challenge1",
        Some("ldb_setup"),
    );
    assert!(url.contains("redirect_uri=urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob"));
    assert!(url.contains("setup_code=ldb_setup"));
    assert!(url.contains("state=state1"));
}

// -----------------------------------------------------------------
// T6: `localdb login --invite <token>`
// -----------------------------------------------------------------

#[tokio::test]
async fn perform_invite_login_open_mode_persists_api_key_and_creates_user() {
    let (dir, state, base_url) = spawn_test_daemon().await;
    let admin = state
        .auth()
        .create_user("admin", Role::Admin)
        .await
        .unwrap();
    let issued = state
        .auth()
        .create_invite(
            localdb_core::auth::InviteMode::Open,
            &[],
            1,
            None,
            &admin.id,
        )
        .await
        .unwrap();
    let ctx = ctx_for(&dir);

    let result = perform_invite_login(
        &ctx,
        &base_url,
        &issued.secret,
        Some("newbie"),
        std::time::Duration::from_millis(10),
    )
    .await;
    assert!(
        result.is_ok(),
        "open-mode invite login should succeed: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap(), "newbie");

    let credentials_file = crate::credentials::credentials_path(&dir.path().join("config.yaml"));
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url)
        .expect("credentials.json must have an entry after invite login");
    assert!(entry.secret.unwrap().starts_with("ldb_"));
    assert!(
        entry.access_token.is_none(),
        "an invite-redeemed API key has no expiry, unlike the OAuth access-token shape"
    );

    let user = state.auth_store().get_user_by_name("newbie").await.unwrap();
    assert!(
        user.is_some(),
        "the invite redemption must have created the user"
    );
}

#[tokio::test]
async fn perform_invite_login_open_mode_wrong_token_fails() {
    let (dir, _state, base_url) = spawn_test_daemon().await;
    let ctx = ctx_for(&dir);

    let result = perform_invite_login(
        &ctx,
        &base_url,
        "ldb_not-a-real-invite",
        Some("someone"),
        std::time::Duration::from_millis(10),
    )
    .await;
    assert!(result.is_err(), "an unknown invite token must fail");
}

#[tokio::test]
async fn perform_invite_login_closed_mode_polls_until_admin_approves() {
    let (dir, state, base_url) = spawn_test_daemon().await;
    let admin = state
        .auth()
        .create_user("admin2", Role::Admin)
        .await
        .unwrap();
    let issued = state
        .auth()
        .create_invite(
            localdb_core::auth::InviteMode::Closed,
            &[],
            1,
            None,
            &admin.id,
        )
        .await
        .unwrap();
    let ctx = ctx_for(&dir);

    // Drive the poll loop with a short interval so this test exercises
    // several real iterations (not just one) before the approval lands.
    let login_task = tokio::spawn({
        let ctx = ctx.clone();
        let base_url = base_url.clone();
        let token = issued.secret.clone();
        async move {
            perform_invite_login(
                &ctx,
                &base_url,
                &token,
                Some("closed-newbie"),
                std::time::Duration::from_millis(20),
            )
            .await
        }
    });

    // Wait for the access request to appear, then let the poll loop run
    // a few iterations before approving.
    let request_id = loop {
        let reqs = state.auth_store().list_access_requests().await.unwrap();
        if let Some(r) = reqs.iter().find(|r| r.requested_name == "closed-newbie") {
            break r.id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    tokio::time::sleep(std::time::Duration::from_millis(70)).await;
    state.auth().approve_request(&request_id).await.unwrap();

    let result = login_task.await.unwrap();
    assert!(
        result.is_ok(),
        "closed-mode invite login should eventually succeed once approved: {:?}",
        result.err()
    );

    let credentials_file = crate::credentials::credentials_path(&dir.path().join("config.yaml"));
    let entry = crate::credentials::lookup_entry(&credentials_file, &base_url)
        .expect("credentials.json must have an entry after approval");
    assert!(entry.secret.unwrap().starts_with("ldb_"));
}

#[tokio::test]
async fn perform_invite_login_closed_mode_denied_fails_with_clear_error() {
    let (dir, state, base_url) = spawn_test_daemon().await;
    let admin = state
        .auth()
        .create_user("admin3", Role::Admin)
        .await
        .unwrap();
    let issued = state
        .auth()
        .create_invite(
            localdb_core::auth::InviteMode::Closed,
            &[],
            1,
            None,
            &admin.id,
        )
        .await
        .unwrap();
    let ctx = ctx_for(&dir);

    let login_task = tokio::spawn({
        let ctx = ctx.clone();
        let base_url = base_url.clone();
        let token = issued.secret.clone();
        async move {
            perform_invite_login(
                &ctx,
                &base_url,
                &token,
                Some("denied-newbie"),
                std::time::Duration::from_millis(10),
            )
            .await
        }
    });

    let request_id = loop {
        let reqs = state.auth_store().list_access_requests().await.unwrap();
        if let Some(r) = reqs.iter().find(|r| r.requested_name == "denied-newbie") {
            break r.id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    state.auth().deny_request(&request_id).await.unwrap();

    let result = login_task.await.unwrap();
    let err = result.expect_err("a denied request must surface as an error");
    assert!(matches!(err, Error::Unauthorized { .. }));
}

#[tokio::test]
async fn invite_duplicate_name_preserves_invalid_request() {
    let (dir, state, base) = spawn_test_daemon().await;
    state
        .auth()
        .create_user("taken", Role::Member)
        .await
        .unwrap();
    let invite = state
        .auth()
        .create_invite(localdb_core::auth::InviteMode::Open, &[], 1, None, "admin")
        .await
        .unwrap();
    let error = perform_invite_login(
        &ctx_for(&dir),
        &base,
        &invite.secret,
        Some("taken"),
        std::time::Duration::from_millis(1),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest { .. }), "{error:?}");
}

#[tokio::test]
async fn polling_stops_on_http_errors_and_malformed_success() {
    for (status, body) in [
        (
            400,
            serde_json::json!({"code":"invalid_request","message":"bad request"}),
        ),
        (
            500,
            serde_json::json!({"code":"internal","message":"unavailable"}),
        ),
        (200, serde_json::json!({"unexpected":true})),
    ] {
        let app = axum::Router::new().fallback(move || async move {
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                axum::Json(body),
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            poll_until_decided(
                &reqwest::Client::new(),
                &base,
                "id",
                "secret",
                std::time::Duration::from_millis(1),
            ),
        )
        .await
        .expect("poll must terminate");
        assert!(result.is_err());
        server.abort();
    }
}

#[tokio::test]
async fn failed_logout_keeps_cache_for_retry() {
    let dir = TempDir::new().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app =
        axum::Router::new().fallback(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR });
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let path = dir.path().join("credentials.json");
    write_credential(
        &path,
        &base,
        CredentialEntry {
            secret: Some("key".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(revoke_and_remove(&path, &base).await.is_err());
    assert_eq!(
        crate::credentials::lookup_secret(&path, &base).as_deref(),
        Some("key")
    );
    server.abort();
    assert!(revoke_and_remove(&path, &base).await.is_err());
    assert!(crate::credentials::lookup_entry(&path, &base).is_some());
}
