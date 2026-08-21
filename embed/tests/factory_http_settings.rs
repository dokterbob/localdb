//! Proves `embed::create_embedder` actually threads the caller's
//! `fetch::http::HttpSettings` into a hosted provider's HTTP client (issue
//! #207 adversarial review, finding 1).
//!
//! Before this fix, `create_openai_compatible`/`create_perplexity`/
//! `create_voyage` each hardcoded `fetch::http::HttpSettings::default()`
//! regardless of what the operator configured under `http:` in
//! `config.yaml` — so a custom `user_agent` or `max_retries` never reached a
//! hosted embedding request, even though every call site already had the
//! parsed `http:` config in scope. `embed/src/factory.rs`'s existing unit
//! tests only ever called `create_embedder` with a default `HttpSettings`
//! (or none at all, pre-fix), so nothing in the suite would have failed if
//! the threading had stayed broken — the parameter could be added and never
//! actually wired to the three `crate::*Embedder::new` calls and every
//! existing test would still pass.
//!
//! These tests close that gap by driving `create_embedder` — the real
//! `embed::create_embedder` entry point every call site in `server`/`cli`
//! uses, not a lower-level constructor — with a non-default `HttpSettings`,
//! then asserting on what a mock HTTP server actually *received on the
//! wire*: the literal `User-Agent` header value, and the literal number of
//! HTTP requests made for a given `max_retries`. A test that merely checked
//! the field was assigned somewhere (a "syntactic" test, per the review)
//! would not have caught the original bug — hardcoding
//! `HttpSettings::default()` still "assigns" a `HttpSettings` value, just
//! always the same one. Only an end-to-end request observation tells the
//! two cases apart.
//!
//! Scoped to the `openai-compatible` provider: it is the only one of the
//! three hosted providers whose `base_url` is configurable via
//! `ProviderConfig` today (`create_perplexity`/`create_voyage` always target
//! their real hosted endpoints — a separate, pre-existing gap, out of scope
//! here), so it is the only one a test can safely redirect at a local mock
//! server instead of the real internet. `create_perplexity`/`create_voyage`
//! call `crate::{Perplexity,Voyage}Embedder::new` with the exact same
//! `http_settings.clone()` shape `create_openai_compatible` does (see
//! `embed/src/factory.rs`), so this test's coverage of the threading itself
//! generalizes; it is the network destination, not the wiring, that differs.

use embed::{create_embedder, DownloadProgress};
use fetch::http::HttpSettings;
use localdb_core::config::schema::{EmbeddingPolicy, ProviderConfig};
use localdb_core::DocumentChunks;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn openai_policy() -> EmbeddingPolicy {
    EmbeddingPolicy {
        provider: "openai-compatible".to_string(),
        model: "text-embedding-3-small".to_string(),
    }
}

fn openai_provider(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        name: "test-openai".to_string(),
        kind: "openai-compatible".to_string(),
        base_url: Some(base_url.to_string()),
        api_key_env: None,
    }
}

fn one_doc() -> Vec<DocumentChunks> {
    vec![DocumentChunks {
        document_context: "irrelevant for the flat openai-compatible path".to_string(),
        chunks: vec!["a chunk".to_string()],
    }]
}

fn embedding_response() -> serde_json::Value {
    serde_json::json!({
        "data": [{"embedding": vec![0.1f32; 1536], "index": 0}]
    })
}

/// A non-default `user_agent` set on the `HttpSettings` passed to
/// `create_embedder` must appear verbatim on the request the built embedder
/// actually sends — not `fetch::http::DEFAULT_USER_AGENT`, which is what a
/// hardcoded `HttpSettings::default()` would have produced instead.
#[tokio::test]
async fn create_embedder_threads_custom_user_agent_to_openai_compatible_request() {
    let server = MockServer::start().await;
    let custom_user_agent = "localdb-test-custom-agent/9.9.9";

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("user-agent", custom_user_agent))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response()))
        .expect(1)
        .mount(&server)
        .await;

    let http_settings = HttpSettings {
        user_agent: Some(custom_user_agent.to_string()),
        ..HttpSettings::default()
    };
    let policy = openai_policy();
    let providers = [openai_provider(&server.uri())];

    let embedder = create_embedder(
        &policy,
        &providers,
        None,
        &http_settings,
        DownloadProgress::Silent,
    )
    .expect("openai-compatible embedder should construct");

    let result = embedder.embed_documents(one_doc()).await;
    assert!(
        result.is_ok(),
        "request should succeed against the mock server: {result:?}"
    );
    // `wiremock`'s `.expect(1)` above (checked on drop) is the real
    // assertion: the mock only matches a request whose `User-Agent` header
    // equals `custom_user_agent`, so a passing test already proves the
    // header reached the wire. The explicit `is_ok()` check just rules out
    // "it failed for an unrelated reason and never actually hit the mock."
}

/// A default (unset) `user_agent` must fall back to
/// `fetch::http::DEFAULT_USER_AGENT`, proving the previous test isn't
/// vacuously matching "any User-Agent" — the mock in this test would reject
/// the custom value from the previous one.
#[tokio::test]
async fn create_embedder_default_http_settings_use_the_shared_default_user_agent() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .and(header("user-agent", fetch::http::DEFAULT_USER_AGENT))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response()))
        .expect(1)
        .mount(&server)
        .await;

    let policy = openai_policy();
    let providers = [openai_provider(&server.uri())];

    let embedder = create_embedder(
        &policy,
        &providers,
        None,
        &HttpSettings::default(),
        DownloadProgress::Silent,
    )
    .expect("openai-compatible embedder should construct");

    let result = embedder.embed_documents(one_doc()).await;
    assert!(
        result.is_ok(),
        "request should succeed against the mock server: {result:?}"
    );
}

/// A non-default `max_retries` must change the literal number of HTTP
/// requests `create_embedder`'s built client makes against a persistently
/// failing endpoint. `min_retry_delay` is dialed to millisecond scale here
/// (a legitimate direct-construction override — see
/// `HttpSettings::min_retry_delay`'s doc comment) purely to keep the test
/// fast; it has no bearing on whether `max_retries` itself threads through.
#[tokio::test]
async fn create_embedder_threads_custom_max_retries_to_openai_compatible_request_count() {
    let server = MockServer::start().await;

    // Every request gets a retryable 500 — the endpoint never succeeds, so
    // the total request count is driven purely by `max_retries`.
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3) // max_retries: 2 => 3 total attempts (1 initial + 2 retries)
        .mount(&server)
        .await;

    let http_settings = HttpSettings {
        max_retries: 2,
        min_retry_delay: std::time::Duration::from_millis(1),
        ..HttpSettings::default()
    };
    let policy = openai_policy();
    let providers = [openai_provider(&server.uri())];

    let embedder = create_embedder(
        &policy,
        &providers,
        None,
        &http_settings,
        DownloadProgress::Silent,
    )
    .expect("openai-compatible embedder should construct");

    let result = embedder.embed_documents(one_doc()).await;
    assert!(
        result.is_err(),
        "endpoint always returns 500, request must ultimately fail"
    );
    // `wiremock`'s `.expect(3)` above is the load-bearing assertion here: if
    // `create_embedder` still hardcoded `HttpSettings::default()`
    // (`max_retries: 3` => 4 attempts), this mock would see a 4th request
    // after it already expects exactly 3, and `wiremock` fails the test on
    // drop with an unexpected-request-count error.
}

/// The other end of the same proof: `max_retries: 0` must make exactly one
/// attempt — no retries at all — against the same always-failing endpoint.
#[tokio::test]
async fn create_embedder_zero_max_retries_makes_exactly_one_request() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let http_settings = HttpSettings {
        max_retries: 0,
        min_retry_delay: std::time::Duration::from_millis(1),
        ..HttpSettings::default()
    };
    let policy = openai_policy();
    let providers = [openai_provider(&server.uri())];

    let embedder = create_embedder(
        &policy,
        &providers,
        None,
        &http_settings,
        DownloadProgress::Silent,
    )
    .expect("openai-compatible embedder should construct");

    let result = embedder.embed_documents(one_doc()).await;
    assert!(result.is_err(), "endpoint always returns 500");
}
