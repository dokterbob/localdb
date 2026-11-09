use super::*;

#[tokio::test]
async fn static_bearer_current_returns_the_configured_secret() {
    let source = StaticBearer("ldb_static".to_string());
    assert_eq!(source.current().await, Some("ldb_static".to_string()));
}

#[tokio::test]
async fn static_bearer_never_refreshes() {
    let source = StaticBearer("ldb_static".to_string());
    assert_eq!(source.refresh("ldb_static").await, None);
}

/// A `BearerSource` whose `refresh` answer is configured up front, so
/// `RefreshingHttpClient::retry_token` can be exercised without a network
/// round trip.
struct ScriptedSource {
    on_refresh: Option<String>,
}

#[async_trait]
impl BearerSource for ScriptedSource {
    async fn current(&self) -> Option<String> {
        None
    }

    async fn refresh(&self, _rejected: &str) -> Option<String> {
        self.on_refresh.clone()
    }
}

fn client_with(on_refresh: Option<&str>) -> RefreshingHttpClient {
    RefreshingHttpClient::new(Arc::new(ScriptedSource {
        on_refresh: on_refresh.map(str::to_string),
    }))
}

#[tokio::test]
async fn retry_token_gives_up_when_no_token_was_sent() {
    // There is nothing to hand the source as `rejected`, so there is
    // nothing worth retrying — e.g. the initial handshake never had a
    // bearer to begin with.
    let client = client_with(Some("ldb_new"));
    assert_eq!(client.retry_token(None).await, None);
}

#[tokio::test]
async fn retry_token_gives_up_when_the_source_cannot_refresh() {
    let client = client_with(None);
    assert_eq!(client.retry_token(Some("ldb_old".to_string())).await, None);
}

#[tokio::test]
async fn retry_token_gives_up_when_the_refresh_echoes_the_rejected_token() {
    // A source that hands back the exact token that was just rejected has
    // made no progress — retrying with it would just fail again.
    let client = client_with(Some("ldb_old"));
    assert_eq!(client.retry_token(Some("ldb_old".to_string())).await, None);
}

#[tokio::test]
async fn retry_token_returns_the_refreshed_token() {
    let client = client_with(Some("ldb_new"));
    assert_eq!(
        client.retry_token(Some("ldb_old".to_string())).await,
        Some("ldb_new".to_string())
    );
}

// -----------------------------------------------------------------
// `get_stream`'s GET request surfaces a 401 as `StreamableHttpError::Client`
// (rmcp's reqwest client calls `response.error_for_status()` there, unlike
// `post_message`, which only recognizes a 401 carrying `WWW-Authenticate`).
// A real axum server stands in for the upstream so the response goes through
// reqwest's own status handling instead of a hand-built error value.
// -----------------------------------------------------------------

/// A `BearerSource` that always offers `initial`, and counts how many times
/// `refresh` is asked to replace a rejected token, answering with
/// `on_refresh` each time.
struct CountingSource {
    initial: String,
    on_refresh: Option<String>,
    refresh_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl BearerSource for CountingSource {
    async fn current(&self) -> Option<String> {
        Some(self.initial.clone())
    }

    async fn refresh(&self, _rejected: &str) -> Option<String> {
        self.refresh_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.on_refresh.clone()
    }
}

/// A GET-only mock upstream that answers a bare 401 (no `WWW-Authenticate`
/// header) unless the request's bearer is exactly `expected_token`, in which
/// case it answers with a minimal `text/event-stream` body.
async fn spawn_sse_upstream(expected_token: String) -> String {
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;

    let app = Router::new().route(
        "/mcp",
        get(move |headers: HeaderMap| {
            let expected_token = expected_token.clone();
            async move {
                let sent = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "));
                if sent == Some(expected_token.as_str()) {
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        "",
                    )
                        .into_response()
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

#[tokio::test]
async fn get_stream_retries_once_on_a_bare_401_and_succeeds() {
    let uri = spawn_sse_upstream("ldb_new".to_string()).await;
    let refresh_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = CountingSource {
        initial: "ldb_old".to_string(),
        on_refresh: Some("ldb_new".to_string()),
        refresh_calls: refresh_calls.clone(),
    };
    let client = RefreshingHttpClient::new(Arc::new(source));

    let result = client
        .get_stream(
            Arc::from(uri.as_str()),
            Some(Arc::from("session-1")),
            None,
            None,
            HashMap::new(),
        )
        .await;

    assert!(
        result.is_ok(),
        "expected the retried GET to succeed once the source hands back the accepted token"
    );
    assert_eq!(
        refresh_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "refresh is attempted exactly once"
    );
}

#[tokio::test]
async fn get_stream_gives_up_when_the_source_cannot_refresh() {
    let uri = spawn_sse_upstream("ldb_new".to_string()).await;
    let refresh_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = CountingSource {
        initial: "ldb_old".to_string(),
        on_refresh: None,
        refresh_calls: refresh_calls.clone(),
    };
    let client = RefreshingHttpClient::new(Arc::new(source));

    let result = client
        .get_stream(
            Arc::from(uri.as_str()),
            Some(Arc::from("session-1")),
            None,
            None,
            HashMap::new(),
        )
        .await;

    match result {
        Err(StreamableHttpError::Client(_)) => {}
        Err(other) => panic!("expected a bare-401 client error, got {other:?}"),
        Ok(_) => panic!("expected the bare 401 to surface once the source declines to refresh"),
    }
    assert_eq!(
        refresh_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "refresh is attempted exactly once, never retried in a loop"
    );
}
