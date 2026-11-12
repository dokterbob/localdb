use super::*;
use axum::response::IntoResponse;
use localdb_core::auth::{AccessRequestState, AuthStore as _, Role};
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

// -----------------------------------------------------------------
// Preflight: a malformed credentials.json must fail before either login
// flow does anything with a one-time side effect (consuming an
// authorization code or an invite's single use).
// -----------------------------------------------------------------

#[tokio::test]
async fn perform_login_fails_preflight_on_malformed_credentials_file_without_opening_browser() {
    let (dir, _state, base_url) = spawn_test_daemon().await;
    let ctx = ctx_for(&dir);
    let credentials_file = crate::credentials::credentials_path(&dir.path().join("config.yaml"));
    std::fs::write(&credentials_file, "not json at all").unwrap();

    let opener_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let opener_called_clone = opener_called.clone();
    let opener = move |_: &str| {
        opener_called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        true
    };

    let result = perform_login(
        &ctx,
        &base_url,
        None,
        false,
        opener,
        std::time::Duration::from_millis(50),
    )
    .await;

    let error = result.expect_err("a malformed credentials file must fail before login starts");
    assert!(
        matches!(error, Error::InvalidConfig { .. }),
        "expected InvalidConfig (exit 2), got {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("credentials.json"),
        "error should name the credentials file: {message}"
    );
    assert!(
        !message.contains("not json at all"),
        "the error must not quote the file's own contents: {message}"
    );
    assert!(
        !opener_called.load(std::sync::atomic::Ordering::SeqCst),
        "the browser must never be opened when the preflight check fails"
    );
}

#[tokio::test]
async fn perform_invite_login_fails_preflight_without_consuming_the_invites_single_use() {
    let (dir, state, base_url) = spawn_test_daemon().await;
    let admin = state
        .auth()
        .create_user("admin-preflight", Role::Admin)
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
    let credentials_file = crate::credentials::credentials_path(&dir.path().join("config.yaml"));
    std::fs::write(&credentials_file, "not json at all").unwrap();

    let result = perform_invite_login(
        &ctx,
        &base_url,
        &issued.secret,
        Some("preflight-user"),
        std::time::Duration::from_millis(10),
    )
    .await;
    let error =
        result.expect_err("a malformed credentials file must fail before redeeming the invite");
    assert!(
        matches!(error, Error::InvalidConfig { .. }),
        "expected InvalidConfig (exit 2), got {error:?}"
    );

    // Remove the malformed file and retry: if the first, failed attempt had
    // already redeemed the invite, its single use would be gone and this
    // would now fail too.
    std::fs::remove_file(&credentials_file).unwrap();
    let retry = perform_invite_login(
        &ctx,
        &base_url,
        &issued.secret,
        Some("preflight-user"),
        std::time::Duration::from_millis(10),
    )
    .await;
    assert!(
        retry.is_ok(),
        "the invite's only use must still be available after the preflight failure: {:?}",
        retry.err()
    );
}

#[tokio::test]
async fn perform_login_with_a_missing_credentials_file_proceeds() {
    // No credentials.json exists yet in this fresh TempDir — the preflight
    // check must treat that as fine and let the flow run, exactly like
    // `perform_login_happy_path_persists_credentials` already demonstrates
    // end to end. This test only pins the preflight's own missing-file case.
    let (dir, state, base_url) = spawn_test_daemon().await;
    let user = state
        .auth()
        .create_user("preflight-missing-file", Role::Admin)
        .await
        .unwrap();
    let api_key = state.auth().issue_api_key(&user.id).await.unwrap().secret;
    let ctx = ctx_for(&dir);

    assert!(
        !crate::credentials::credentials_path(&dir.path().join("config.yaml")).exists(),
        "precondition: no credentials.json yet"
    );

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
    assert!(
        result.is_ok(),
        "a missing credentials file must not block login: {:?}",
        result.err()
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
// `localdb login --invite <token>`
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
    let (joined_name, bearer) = result.unwrap();
    assert_eq!(joined_name, "newbie");
    assert!(
        !bearer.is_empty(),
        "the login's own bearer must be returned"
    );

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

    // Wait for the access request to appear, failing fast (rather than
    // hanging) if the login task exits before it does — that would mean the
    // request was never created, not that we should keep waiting.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let request_id = loop {
        if login_task.is_finished() {
            let result = login_task.await;
            panic!("login task finished before its access request appeared: {result:?}");
        }
        let reqs = state.auth_store().list_access_requests().await.unwrap();
        if let Some(r) = reqs.iter().find(|r| r.requested_name == "closed-newbie") {
            break r.id.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the access request to appear"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };

    // Let the poll loop run a few real iterations before approving: observe
    // the request actually staying `Pending` (and the login task still
    // running) across a few poll_interval-spaced checks, instead of
    // sleeping a fixed, arbitrary duration.
    for _ in 0..3 {
        if login_task.is_finished() {
            let result = login_task.await;
            panic!("login task finished before approval: {result:?}");
        }
        let reqs = state.auth_store().list_access_requests().await.unwrap();
        let still_pending = reqs
            .iter()
            .any(|r| r.id == request_id && r.state == AccessRequestState::Pending);
        assert!(
            still_pending,
            "access request should still be pending before approval"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
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

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let request_id = loop {
        if login_task.is_finished() {
            let result = login_task.await;
            panic!("login task finished before its access request appeared: {result:?}");
        }
        let reqs = state.auth_store().list_access_requests().await.unwrap();
        if let Some(r) = reqs.iter().find(|r| r.requested_name == "denied-newbie") {
            break r.id.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the access request to appear"
        );
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
async fn poll_until_decided_sends_the_secret_in_the_json_body_not_the_url() {
    let recorded: Arc<std::sync::Mutex<Option<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(None));
    let recorded_clone = recorded.clone();

    let app = axum::Router::new().route(
        "/v1/invites/requests/{id}/poll",
        axum::routing::post(move |uri: axum::http::Uri, body: axum::body::Bytes| {
            let recorded = recorded_clone.clone();
            async move {
                *recorded.lock().unwrap() =
                    Some((uri.to_string(), String::from_utf8_lossy(&body).into_owned()));
                axum::Json(serde_json::json!({ "state": "approved", "api_key": "ldb_minted" }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // A request id with a space, to prove it's percent-encoded as a path
    // segment rather than dropped into the URL raw.
    let result = poll_until_decided(
        &reqwest::Client::new(),
        &base,
        "req id",
        "ldb_topsecret",
        std::time::Duration::from_millis(1),
    )
    .await;
    assert_eq!(result.unwrap(), "ldb_minted");

    let (uri, body) = recorded
        .lock()
        .unwrap()
        .clone()
        .expect("the mock daemon must have recorded exactly one poll request");
    assert!(
        !uri.contains('?'),
        "the poll request's URI must carry no query string: {uri}"
    );
    assert!(
        !uri.contains("ldb_topsecret"),
        "the secret must never appear in the URI: {uri}"
    );
    assert!(
        uri.contains("req%20id"),
        "the request id must be percent-encoded as a path segment: {uri}"
    );

    let json_body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json_body["secret"], "ldb_topsecret");

    server.abort();
}

/// A `429` from the daemon's per-IP rate limiter (specs/05-surfaces.md
/// §3.1) must not end the poll loop as a failure — `poll_until_decided`
/// sleeps the given `Retry-After` and keeps polling, so a login that
/// happens to collide with another concurrent login sharing the same IP
/// still completes.
#[tokio::test]
async fn poll_until_decided_survives_a_429_and_keeps_polling() {
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let call_count_clone = call_count.clone();

    let app = axum::Router::new().route(
        "/v1/invites/requests/{id}/poll",
        axum::routing::post(move |_body: axum::body::Bytes| {
            let call_count = call_count_clone.clone();
            async move {
                let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        [("retry-after", "0")],
                        axum::Json(serde_json::json!({
                            "code": "rate_limited",
                            "message": "too many requests"
                        })),
                    )
                        .into_response()
                } else {
                    axum::Json(serde_json::json!({ "state": "approved", "api_key": "ldb_minted" }))
                        .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        poll_until_decided(
            &reqwest::Client::new(),
            &base,
            "req-id",
            "ldb_topsecret",
            std::time::Duration::from_millis(1),
        ),
    )
    .await
    .expect("poll must terminate");

    assert_eq!(
        result.unwrap(),
        "ldb_minted",
        "the loop must survive the 429 and collect the key on the next poll"
    );
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "exactly one 429 then one success"
    );

    server.abort();
}

// -----------------------------------------------------------------
// Bounding `poll_until_decided`'s handling of a `429`'s `Retry-After`: a
// hostile or merely broken value must not stall login indefinitely (a huge
// value) or hot-loop (zero), and repeated `429`s must eventually give up
// rather than extend the wait forever. Paused tokio time
// (`start_paused = true`) makes the huge-wait case assertable without a
// real minutes-long sleep — the runtime auto-advances the virtual clock
// past a `tokio::time::sleep` once nothing else is runnable, so a 60s
// clamped wait resolves instantly in real time while still measuring 60
// virtual seconds elapsed.
// -----------------------------------------------------------------

/// Spawn a mock poll endpoint returning one `429` (with the given
/// `Retry-After` header value, if any) followed by an approval — the shape
/// every test below needs, just varying the header.
async fn spawn_single_429_then_approved_mock(
    retry_after: Option<&'static str>,
) -> (String, tokio::task::JoinHandle<()>) {
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/v1/invites/requests/{id}/poll",
        axum::routing::post(move |_body: axum::body::Bytes| {
            let call_count = call_count.clone();
            async move {
                let n = call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    let mut response = (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(serde_json::json!({
                            "code": "rate_limited",
                            "message": "too many requests"
                        })),
                    )
                        .into_response();
                    if let Some(ra) = retry_after {
                        response.headers_mut().insert(
                            axum::http::header::RETRY_AFTER,
                            axum::http::HeaderValue::from_static(ra),
                        );
                    }
                    response
                } else {
                    axum::Json(serde_json::json!({ "state": "approved", "api_key": "ldb_minted" }))
                        .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, server)
}

#[tokio::test(start_paused = true)]
async fn poll_until_decided_clamps_a_huge_retry_after() {
    let (base, server) = spawn_single_429_then_approved_mock(Some("315360000")).await;
    let poll_interval = std::time::Duration::from_millis(100);

    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        MAX_RATE_LIMIT_WAIT * 10,
        poll_until_decided(
            &reqwest::Client::new(),
            &base,
            "req-id",
            "secret",
            poll_interval,
        ),
    )
    .await
    .expect("poll must terminate");
    let elapsed = started.elapsed();

    assert_eq!(result.unwrap(), "ldb_minted");
    assert!(
        elapsed <= MAX_RATE_LIMIT_WAIT,
        "a huge Retry-After (315360000s) must be clamped to MAX_RATE_LIMIT_WAIT \
         ({MAX_RATE_LIMIT_WAIT:?}), not honored literally: waited {elapsed:?}"
    );
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn poll_until_decided_clamps_a_zero_retry_after_to_the_poll_interval() {
    let (base, server) = spawn_single_429_then_approved_mock(Some("0")).await;
    let poll_interval = std::time::Duration::from_millis(100);

    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        poll_until_decided(
            &reqwest::Client::new(),
            &base,
            "req-id",
            "secret",
            poll_interval,
        ),
    )
    .await
    .expect("poll must terminate");
    let elapsed = started.elapsed();

    assert_eq!(result.unwrap(), "ldb_minted");
    assert!(
        elapsed >= poll_interval,
        "Retry-After: 0 must be clamped up to the poll interval, not hot-loop: waited {elapsed:?}"
    );
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn poll_until_decided_with_a_missing_retry_after_falls_back_to_the_poll_interval() {
    let (base, server) = spawn_single_429_then_approved_mock(None).await;
    let poll_interval = std::time::Duration::from_millis(100);

    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        poll_until_decided(
            &reqwest::Client::new(),
            &base,
            "req-id",
            "secret",
            poll_interval,
        ),
    )
    .await
    .expect("poll must terminate");
    let elapsed = started.elapsed();

    assert_eq!(result.unwrap(), "ldb_minted");
    assert!(
        elapsed >= poll_interval,
        "a missing Retry-After must fall back to the poll interval: waited {elapsed:?}"
    );
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn poll_until_decided_gives_up_after_too_many_consecutive_429s() {
    // Every call is a 429 — the request is never decided, only ever
    // throttled. The loop must give up rather than retry forever.
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let call_count_clone = call_count.clone();
    let app = axum::Router::new().route(
        "/v1/invites/requests/{id}/poll",
        axum::routing::post(move |_body: axum::body::Bytes| {
            let call_count = call_count_clone.clone();
            async move {
                call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "1")],
                    axum::Json(serde_json::json!({
                        "code": "rate_limited",
                        "message": "too many requests"
                    })),
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        poll_until_decided(
            &reqwest::Client::new(),
            &base,
            "req-id",
            "secret",
            std::time::Duration::from_millis(1),
        ),
    )
    .await
    .expect("poll must terminate, not hang forever on endless 429s");

    let err = result.expect_err("endless 429s must eventually give up with an error");
    assert!(
        matches!(err, Error::RateLimited { .. }),
        "expected RateLimited (exit 5, unavailable-class), got {err:?}"
    );
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst) as u32,
        MAX_CONSECUTIVE_RATE_LIMITS + 1,
        "the loop must give up exactly once MAX_CONSECUTIVE_RATE_LIMITS is exceeded"
    );
    server.abort();
}

// -----------------------------------------------------------------
// `GET /v1/auth/me` identity fetch (login success reports identity)
// -----------------------------------------------------------------

#[tokio::test]
async fn fetch_identity_returns_name_and_role_for_a_valid_bearer() {
    let (_dir, state, base_url) = spawn_test_daemon().await;
    let user = state
        .auth()
        .create_user("identity-admin", Role::Admin)
        .await
        .unwrap();
    let api_key = state.auth().issue_api_key(&user.id).await.unwrap().secret;

    let (name, role) = fetch_identity(&base_url, &api_key).await.unwrap();
    assert_eq!(name, "identity-admin");
    assert_eq!(role, "admin");
}

#[tokio::test]
async fn fetch_identity_fails_for_an_invalid_bearer() {
    let (_dir, _state, base_url) = spawn_test_daemon().await;
    let result = fetch_identity(&base_url, "ldb_not-a-real-key").await;
    assert!(result.is_err());
}

/// A login's identity lookup is best-effort: the credential is already
/// persisted by the time it runs, so a failure (here: an invalid bearer,
/// standing in for a timeout/error status/malformed body) must surface as
/// "no identity" rather than an error the caller would have to propagate
/// into a non-zero exit. Drives `build_login_result` — the function
/// `report_login_success` actually calls — rather than the lower-level
/// `resolve_identity_best_effort`, so this would fail if the result
/// building ever made a missing identity fatal.
#[tokio::test]
async fn identity_lookup_failure_after_persisted_login_yields_no_identity_not_an_error() {
    let (dir, _state, base_url) = spawn_test_daemon().await;
    let ctx = ctx_for(&dir);
    let result = build_login_result(&ctx, &base_url, "ldb_not-a-real-key").await;
    assert_eq!(result.base_url, base_url);
    assert!(
        result.name.is_none() && result.role.is_none(),
        "a failed identity lookup must not be treated as a fatal error, and must not report a name/role it could not confirm"
    );
}

/// The identity reported after login must reflect the bearer the login
/// flow itself just obtained — never a different credential sitting in
/// `ctx.api_key` (the CLI's stand-in for `LOCALDB_API_KEY`/`--api-key`).
/// Logging in as one user while a *different* user's key is set in the
/// environment must still report the user who just logged in. Drives
/// `build_login_result` directly (the same function the invite login path
/// also reaches, via `report_login_success`) so this would fail if the
/// result building ever went back to resolving identity through
/// `ctx.api_key` instead of the login's own bearer.
#[tokio::test]
async fn login_bearer_is_used_for_identity_not_an_unrelated_env_api_key() {
    let (dir, state, base_url) = spawn_test_daemon().await;
    let alice = state
        .auth()
        .create_user("alice-just-logged-in", Role::Admin)
        .await
        .unwrap();
    let alice_key = state.auth().issue_api_key(&alice.id).await.unwrap().secret;

    let bob = state
        .auth()
        .create_user("bob-env-key", Role::Member)
        .await
        .unwrap();
    let bob_key = state.auth().issue_api_key(&bob.id).await.unwrap().secret;

    let mut ctx = ctx_for(&dir);
    ctx.api_key = Some(bob_key);

    let opener = fake_browser_opener(alice_key);
    let bearer = perform_login(
        &ctx,
        &base_url,
        None,
        false,
        opener,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("login as alice should succeed even with bob's key set as ctx.api_key");

    let result = build_login_result(&ctx, &base_url, &bearer).await;
    assert_eq!(
        result.name.as_deref(),
        Some("alice-just-logged-in"),
        "identity must reflect the bearer login just obtained, not ctx.api_key"
    );
    assert_eq!(result.role.as_deref(), Some("admin"));
}

// -----------------------------------------------------------------
// `/token` error classification (5xx must not report as Unauthorized)
// -----------------------------------------------------------------

#[test]
fn classify_token_error_5xx_is_not_unauthorized() {
    for status in [500, 502, 503] {
        let error = classify_token_error(
            reqwest::StatusCode::from_u16(status).unwrap(),
            "boom".to_string(),
        );
        assert!(
            !matches!(error, Error::Unauthorized { .. }),
            "a {status} /token response must not classify as Unauthorized: {error:?}"
        );
    }
}

#[test]
fn classify_token_error_4xx_oauth_errors_are_unauthorized() {
    for status in [400, 401] {
        let error = classify_token_error(
            reqwest::StatusCode::from_u16(status).unwrap(),
            "invalid_grant".to_string(),
        );
        assert!(
            matches!(error, Error::Unauthorized { .. }),
            "a {status} /token response must classify as Unauthorized: {error:?}"
        );
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
