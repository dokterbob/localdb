//! Rejected-request logging tests.

use crate::daemon::build_router;

use super::common::make_state;

// --- rejected-request logging (issue #147) ---
//
// A minimal `MakeWriter` capturing formatted log lines into a shared
// buffer, installed via `tracing::subscriber::set_default` — scoped to
// the current thread/task rather than global `tracing::subscriber::set_global_default`,
// so it can't clash with other tests' subscribers running in parallel
// (`cargo test` runs each test in its own thread by default, and each
// `#[tokio::test]` here uses a single-threaded current-thread runtime,
// so the thread-local default set before `.await`ing stays in effect for
// the whole request).

#[derive(Clone, Default)]
struct BufWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// A request that 4xxs (an unknown path -> axum's default 404) must
/// produce a WARN-level log line carrying method, path, and status —
/// proving the middleware installed in `build_router` actually observes
/// rejected responses instead of leaving them undiagnosable (issue #147).
#[tokio::test]
async fn rejected_response_is_logged_at_warn() {
    let (_dir, state) = make_state().await;
    let app = build_router(
        state,
        vec![],
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    let buf = BufWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);

    drop(_guard);

    let captured = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        captured.contains("WARN"),
        "expected a WARN line, captured: {captured}"
    );
    assert!(
        captured.contains("GET"),
        "expected the method in the log line, captured: {captured}"
    );
    assert!(
        captured.contains("/v1/does-not-exist"),
        "expected the path in the log line, captured: {captured}"
    );
    assert!(
        captured.contains("404"),
        "expected the status in the log line, captured: {captured}"
    );
}

/// The 404 case above only proves the `log_rejected_responses` layer
/// sees rejections from the top-level `/v1` routes — it never reaches
/// the nested `/mcp` mount at all (no route matches
/// `/v1/does-not-exist`). `log_rejected_responses` is deliberately
/// applied *after* `nest_service` specifically so it also wraps
/// rejections rmcp's own `StreamableHttpService` produces internally
/// (see `build_router`'s doc comment) — this test drives an actual
/// request through the mount and proves that half of the claim.
///
/// The deterministic rejection: rmcp's Streamable HTTP transport
/// validates the `MCP-Protocol-Version` header on every request/method
/// before any session/handshake state is needed (rmcp
/// `validate_protocol_version_header`) — an unsupported version always
/// 400s, with no need to first complete an `initialize` round trip or
/// open a session, unlike almost every other way `/mcp` can reject a
/// request.
#[tokio::test]
async fn rejected_response_through_mcp_mount_is_logged_at_warn() {
    let (_dir, state) = make_state().await;
    let app = build_router(
        state,
        vec![],
        std::sync::Arc::new(localdb_core::FakeEmbedder::new(1)),
        vec![],
    );

    let buf = BufWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/mcp")
                .header("MCP-Protocol-Version", "not-a-real-version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::BAD_REQUEST,
        "an unsupported MCP-Protocol-Version should 400 before any session is needed"
    );

    drop(_guard);

    let captured = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        captured.contains("WARN"),
        "expected a WARN line for a rejection inside the /mcp mount, captured: {captured}"
    );
    assert!(
        captured.contains("/mcp"),
        "expected the /mcp path in the log line, captured: {captured}"
    );
    assert!(
        captured.contains("400"),
        "expected the status in the log line, captured: {captured}"
    );
}
