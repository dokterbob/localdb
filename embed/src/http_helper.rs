//! Shared HTTP retry helper for hosted embedding providers.
//!
//! Retries are driven by `fetch::http`'s shared, `backon`-based outgoing-HTTP
//! retry policy (issue #207) instead of a second, hand-rolled loop living
//! only in this crate. The old loop (see `crate::retry`'s module doc for the
//! full history) never honored a server's `Retry-After` header, had no
//! jitter, and computed its exponential curve with an integer-seconds bug
//! that silently zeroed out any sub-second `initial_backoff`. Reusing
//! `fetch::http` fixes all three for hosted embedding providers the same way
//! it already does for document fetches — this module's remaining job is
//! adapting that generic retry machinery to embed's request/response shape
//! (headers + body in, raw bytes out) and to [`EmbedError`]. It also holds
//! the hosted providers' shared constructor plumbing (client construction,
//! and — for the two document-context providers — default resolution): see
//! [`build_hosted_client`] and [`resolve_contextual_defaults`].

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use backon::Retryable;
use fetch::http::{self, HttpSettings, RetryError};
use tracing::warn;

use crate::error::EmbedError;
use crate::retry::RetryPolicy;

const PROVIDER: &str = "hosted-http";

/// Build the `reqwest::Client` shared by every hosted embedding provider's
/// constructor (`OpenAiEmbedder`, `PerplexityEmbedder`, `VoyageEmbedder`).
///
/// All three build their client the same way — `fetch::http::client_builder`
/// applied to the operator's `http_settings`, with `RetryPolicy::
/// request_timeout` as the per-request timeout — and map a build failure to
/// the same `EmbedError::Internal` message. Pulled out so that shape is
/// written once instead of drifting across three near-identical `new()`
/// bodies.
pub(crate) fn build_hosted_client(
    http_settings: &HttpSettings,
    request_timeout: Duration,
) -> Result<reqwest::Client, EmbedError> {
    http::client_builder(http_settings)
        .timeout(request_timeout)
        .build()
        .map_err(|e| EmbedError::Internal(format!("failed to build HTTP client: {e}")))
}

/// Resolve a document-context hosted provider's constructor inputs
/// (`PerplexityEmbedder::new`, `VoyageEmbedder::new`): apply the `model`/
/// `embedding_dim` defaults and build the shared HTTP client in one call.
///
/// Perplexity and Voyage differ only in their default base URL/model/
/// dimension and their request/response JSON shape — their `new()` bodies
/// were otherwise byte-for-byte identical, which is what this helper
/// collapses. `OpenAiEmbedder` doesn't use this: its flat-provider `new()`
/// takes a non-optional `model` (no default to apply) and retains extra
/// fields (`dimensions`, `retry`) neither contextual provider has, so it
/// only shares [`build_hosted_client`] with these two, not this wrapper.
pub(crate) fn resolve_contextual_defaults(
    http_settings: &HttpSettings,
    retry: &RetryPolicy,
    model: Option<String>,
    default_model: &str,
    embedding_dim: Option<usize>,
    default_dim: usize,
) -> Result<(reqwest::Client, String, usize), EmbedError> {
    let model = model.unwrap_or_else(|| default_model.to_string());
    let embedding_dim = embedding_dim.unwrap_or(default_dim);
    let client = build_hosted_client(http_settings, retry.request_timeout)?;
    Ok((client, model, embedding_dim))
}

/// Send an HTTP POST request, retrying transient failures per `settings`.
///
/// Honors a response's `Retry-After` header (capped at
/// [`http::INLINE_RETRY_AFTER_CAP`]) when present, otherwise backs off along
/// `fetch::http::retry_policy`'s jittered exponential curve. Retryable vs.
/// fatal outcomes are classified by `fetch::http::is_transient` — HTTP
/// 429/408/5xx and transient network errors (timeout, connect failure) are
/// retried; every other status or network error fails on the first attempt.
///
/// # Errors
/// Returns [`EmbedError::ProviderError`] for a non-retryable failure (e.g. a
/// 400 or 401 response), or [`EmbedError::RetriesExhausted`] once
/// `settings.max_retries` — or the retry schedule's total-delay budget — is
/// exhausted on an otherwise-retryable failure.
pub async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
    settings: &HttpSettings,
) -> Result<Vec<u8>, EmbedError> {
    // Interior-mutable side channel: the retried closure below is `FnMut`
    // called by value on each attempt, but the final error mapping after the
    // loop needs to know how many attempts actually ran and what the most
    // recent non-2xx response body said. Both are read only after `.await`
    // below completes, once the closure has stopped running. `Cell`/`RefCell`
    // would be simpler, but `#[async_trait]` (used by every `Embedder` impl
    // that calls this helper) boxes its futures as `dyn Future + Send`, and a
    // `&Cell`/`&RefCell` held across an `.await` is not `Send` because
    // neither type is `Sync` — hence the `Sync`-safe `Atomic`/`Mutex`
    // versions here even though nothing actually runs concurrently.
    let attempts = AtomicU32::new(0);
    let last_message = Mutex::new(String::new());

    let attempt = || {
        attempts.fetch_add(1, Ordering::Relaxed);
        async {
            let response = client
                .post(url)
                .headers(headers.clone())
                .body(body.clone())
                .send()
                .await
                .map_err(RetryError::Request)?;

            let status = response.status();
            if status.is_success() {
                return response
                    .bytes()
                    .await
                    .map(|bytes| bytes.to_vec())
                    .map_err(RetryError::Request);
            }

            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(http::parse_retry_after);
            let body_text = response.text().await.unwrap_or_default();
            // `status.as_u16()`, not `{status}` — `StatusCode`'s `Display`
            // prints the canonical reason phrase too ("400 Bad Request"),
            // which would make this message diverge from the bare numeric
            // code callers and tests expect ("HTTP 400: ...").
            *last_message
                .lock()
                .expect("last_message mutex is never held across a panic") =
                format!("HTTP {}: {body_text}", status.as_u16());

            Err(RetryError::Status {
                status,
                retry_after,
            })
        }
    };

    let outcome = attempt
        .retry(http::retry_policy(settings))
        .when(http::is_transient)
        // Shared with `fetch::HttpUrlFetcher::fetch` via `fetch::http` rather
        // than duplicated here — see `http::retry_after_adjuster`'s doc
        // comment for the full contract (the `dur?` short-circuit inside it
        // is load-bearing: an earlier version of this closure that skipped
        // it resurrected a stopped retry loop against a server that kept
        // sending 429 + Retry-After, an observed infinite loop, not a
        // hypothetical one).
        //
        // Unlike `fetch::lib`'s call site, this crate never records a
        // `Retry-After` value as a pacing cooldown (see this module's own
        // doc comment: hosted embedding providers get no proactive per-host
        // limiter) — so an oversized `Retry-After` ending the loop here
        // loses nothing extra beyond the terminal error itself.
        .adjust(http::retry_after_adjuster(http::total_retry_budget(
            settings,
        )))
        .notify(|err: &RetryError, dur: Duration| {
            warn!(url = %url, wait = ?dur, error = ?err, "hosted embedding request failed, retrying");
        })
        .await;

    let attempts = attempts.load(Ordering::Relaxed);
    match outcome {
        Ok(bytes) => Ok(bytes),
        Err(RetryError::Status { status, .. }) => {
            let message = last_message
                .into_inner()
                .expect("last_message mutex is never held across a panic");
            // A status `is_transient` classified `false` never reached the
            // retry loop's `.when()` more than once — it failed fast on the
            // first attempt, so it is a fatal provider error, not an
            // exhausted retry budget.
            if http::is_transient(&RetryError::Status {
                status,
                retry_after: None,
            }) {
                Err(EmbedError::RetriesExhausted {
                    provider: PROVIDER.to_string(),
                    attempts,
                    last_error: message,
                })
            } else {
                Err(EmbedError::ProviderError {
                    provider: PROVIDER.to_string(),
                    message,
                })
            }
        }
        Err(RetryError::Request(e)) => Err(EmbedError::RetriesExhausted {
            provider: PROVIDER.to_string(),
            attempts,
            last_error: e.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, CONTENT_TYPE};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Settings for tests that force a retry: `min_retry_delay` (and, via
    /// `fetch::http::retry_policy`, the derived `max_delay`/`total_delay`) is
    /// dialed down to millisecond scale so a computed (non-`Retry-After`)
    /// backoff never adds more than a few milliseconds of real sleep, no
    /// matter how many retries a test forces. See `fetch::http::HttpSettings::
    /// min_retry_delay`'s doc comment — this is exactly the test seam it
    /// exists for.
    fn test_settings(max_retries: u32) -> HttpSettings {
        HttpSettings {
            max_retries,
            min_retry_delay: Duration::from_millis(1),
            ..HttpSettings::default()
        }
    }

    fn json_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            "application/json".parse().expect("valid header value"),
        );
        headers
    }

    #[tokio::test]
    async fn send_with_retry_returns_body_when_status_success() {
        // Given: a hosted provider endpoint that accepts the first request.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"ok\":true}"))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        // When: the helper sends a JSON request.
        let body = send_with_retry(
            &client,
            &format!("{}/embeddings", server.uri()),
            json_headers(),
            br#"{"input":["a"]}"#.to_vec(),
            &test_settings(2),
        )
        .await
        .expect("successful response should return body bytes");

        // Then: the raw response bytes are returned for caller-owned parsing.
        assert_eq!(body, br#"{"ok":true}"#.to_vec());
    }

    #[tokio::test]
    async fn send_with_retry_retries_retryable_status_then_returns_body() {
        // Given: a provider endpoint that rate-limits once, then succeeds.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"retried\":true}"))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        // When: the helper receives a retryable status before attempts are exhausted.
        let body = send_with_retry(
            &client,
            &format!("{}/embeddings", server.uri()),
            json_headers(),
            br#"{"input":["a"]}"#.to_vec(),
            &test_settings(2),
        )
        .await
        .expect("retryable status should be retried");

        // Then: the successful retry body is returned.
        assert_eq!(body, br#"{"retried":true}"#.to_vec());
    }

    #[tokio::test]
    async fn send_with_retry_fails_fast_when_status_is_non_retryable_4xx() {
        // Given: a provider endpoint that rejects the request with a non-retryable 4xx.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        // When: the helper receives the non-retryable response.
        let error = send_with_retry(
            &client,
            &format!("{}/embeddings", server.uri()),
            json_headers(),
            br#"{"input":["a"]}"#.to_vec(),
            &test_settings(3),
        )
        .await
        .expect_err("400 should fail without retrying");

        // Then: callers receive the provider status and response body, and it
        // is classified as a fatal provider error, not an exhausted retry.
        assert!(error.to_string().contains("HTTP 400: bad request"));
        assert!(matches!(error, EmbedError::ProviderError { .. }));
    }

    /// New behavior (issue #207): the old hand-rolled loop never looked at
    /// `Retry-After` at all. This proves it is honored end to end — a 1s
    /// hint is slept on inline rather than falling back to the (much
    /// shorter, at `test_settings`' scale) computed backoff.
    #[tokio::test]
    async fn send_with_retry_honors_retry_after_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"ok\":true}"))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        let start = std::time::Instant::now();
        let body = send_with_retry(
            &client,
            &format!("{}/embeddings", server.uri()),
            json_headers(),
            br#"{"input":["a"]}"#.to_vec(),
            &test_settings(2),
        )
        .await
        .expect("must eventually succeed");
        let elapsed = start.elapsed();

        assert_eq!(body, br#"{"ok":true}"#.to_vec());
        assert!(
            elapsed >= Duration::from_millis(900),
            "a 1s Retry-After should be honored inline, waited only {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "wait must stay bounded, got {elapsed:?}"
        );
    }

    /// New behavior (issue #207): retries exhausted on a persistently-429
    /// endpoint produce `EmbedError::RetriesExhausted`, not a hang or a
    /// generic provider error — the old loop had no way to distinguish
    /// "server keeps rate-limiting us" from "server is broken" either, since
    /// it never modeled `Retry-After` at all.
    #[tokio::test]
    async fn send_with_retry_exhausted_returns_retries_exhausted() {
        let server = MockServer::start().await;
        // max_retries=1 means 2 total attempts; pin that exactly.
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429).set_body_string("still limited"))
            .expect(2)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        let error = send_with_retry(
            &client,
            &format!("{}/embeddings", server.uri()),
            json_headers(),
            br#"{"input":["a"]}"#.to_vec(),
            &test_settings(1),
        )
        .await
        .expect_err("persistent 429s should exhaust retries");

        match error {
            EmbedError::RetriesExhausted {
                attempts,
                last_error,
                ..
            } => {
                assert_eq!(attempts, 2, "1 retry configured => 2 total attempts");
                assert!(
                    last_error.contains("still limited"),
                    "last_error should carry the final response body: {last_error}"
                );
            }
            other => panic!("expected RetriesExhausted, got {other:?}"),
        }
    }

    /// Defect 1 regression (issue #207 follow-up), embed-side mirror of
    /// `fetch::lib`'s `test_retry_after_over_cap_ends_fetch_immediately_
    /// with_no_inline_sleep`: a `Retry-After` over `http::INLINE_RETRY_
    /// AFTER_CAP` (30s) used to be clamped to the cap and slept on inline
    /// anyway. It must instead end the request immediately as
    /// `RetriesExhausted` — no inline sleep, and exactly one request
    /// (`.expect(1)` fails the test if a retry goes out). `max_retries` is
    /// generous (5) so the assertion pins the oversized `Retry-After` as the
    /// reason the loop stopped, not exhausted retries.
    #[tokio::test]
    async fn send_with_retry_over_cap_retry_after_ends_immediately_with_no_inline_sleep() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "40"))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        let start = std::time::Instant::now();
        let error = send_with_retry(
            &client,
            &format!("{}/embeddings", server.uri()),
            json_headers(),
            br#"{"input":["a"]}"#.to_vec(),
            &test_settings(5),
        )
        .await
        .expect_err("an over-cap Retry-After must not be retried into success");
        let elapsed = start.elapsed();

        assert!(
            matches!(error, EmbedError::RetriesExhausted { attempts: 1, .. }),
            "expected RetriesExhausted after exactly 1 attempt, got {error:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "an over-cap Retry-After must never be slept on inline, took {elapsed:?}"
        );
    }

    /// Defect 2 regression (issue #207 follow-up), embed-side mirror of
    /// `fetch::lib`'s `test_retry_after_cumulative_sleep_is_bounded_by_
    /// total_budget`. See that test's doc comment for the full numeric
    /// derivation; the shape here is identical since both crates share
    /// `fetch::http::retry_after_adjuster`: `min_retry_delay` 50ms ×
    /// `TOTAL_DELAY_RATIO` 30 = 1.5s total budget, a 1s `Retry-After` on
    /// every response, `max_retries` generous (10) so the fix — not
    /// `max_retries` running out — is what bounds the loop to ~2s/3
    /// requests instead of running for several more retries at 1s each.
    #[tokio::test]
    async fn send_with_retry_cumulative_sleep_is_bounded_by_total_budget() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        let settings = HttpSettings {
            max_retries: 10,
            min_retry_delay: Duration::from_millis(50),
            ..HttpSettings::default()
        };
        let start = std::time::Instant::now();
        let error = send_with_retry(
            &client,
            &format!("{}/embeddings", server.uri()),
            json_headers(),
            br#"{"input":["a"]}"#.to_vec(),
            &settings,
        )
        .await
        .expect_err("persistent 429s should exhaust the retry budget");
        let elapsed = start.elapsed();
        let requests = server.received_requests().await.unwrap_or_default().len();

        assert!(
            matches!(error, EmbedError::RetriesExhausted { .. }),
            "expected RetriesExhausted, got {error:?}"
        );
        assert!(
            requests <= 4,
            "cumulative honored Retry-After sleep should stop the loop well \
             before max_retries (10) is reached; got {requests} requests"
        );
        assert!(
            elapsed < Duration::from_millis(2500),
            "cumulative sleep must stay near the ~1.5s budget; got {elapsed:?}"
        );
    }
}
