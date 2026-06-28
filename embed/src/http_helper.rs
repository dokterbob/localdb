//! Shared HTTP retry helper for hosted embedding providers.

use crate::error::EmbedError;
use crate::retry::RetryPolicy;
use tracing::{debug, warn};

const PROVIDER: &str = "hosted-http";

/// Send an HTTP POST request with the hosted-provider retry policy.
///
/// # Errors
/// Returns [`EmbedError`] when the request fails, a non-retryable status is
/// returned, or all retry attempts are exhausted.
pub async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
    policy: &RetryPolicy,
) -> Result<Vec<u8>, EmbedError> {
    let max_attempts = policy.max_attempts.max(1);
    let mut last_error = String::new();

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let backoff = policy.backoff_for_attempt(attempt - 1);
            debug!(
                attempt,
                backoff_ms = backoff.as_millis(),
                "retrying hosted embedding request"
            );
            tokio::time::sleep(backoff).await;
        }

        let request = client
            .post(url)
            .headers(headers.clone())
            .body(body.clone())
            .send()
            .await;

        match request {
            Ok(response) if response.status().is_success() => {
                return response
                    .bytes()
                    .await
                    .map(|bytes| bytes.to_vec())
                    .map_err(EmbedError::Http);
            }
            Ok(response) => {
                let status = response.status().as_u16();
                let body_text = response.text().await.unwrap_or_default();
                let error = format!("HTTP {status}: {body_text}");

                if should_retry_status(policy, status) && attempt + 1 < max_attempts {
                    warn!(status, "hosted embedding request failed, will retry");
                    last_error = error;
                    continue;
                }

                if should_retry_status(policy, status) {
                    return Err(EmbedError::RetriesExhausted {
                        provider: PROVIDER.to_string(),
                        attempts: attempt + 1,
                        last_error: error,
                    });
                }

                return Err(EmbedError::ProviderError {
                    provider: PROVIDER.to_string(),
                    message: error,
                });
            }
            Err(error) if error.is_timeout() => {
                warn!("hosted embedding request timed out");
                if attempt + 1 >= max_attempts {
                    return Err(EmbedError::Timeout {
                        provider: PROVIDER.to_string(),
                        timeout_secs: policy.request_timeout.as_secs(),
                    });
                }
                last_error = error.to_string();
            }
            Err(error) => {
                warn!(%error, "hosted embedding request failed");
                if attempt + 1 >= max_attempts {
                    return Err(EmbedError::RetriesExhausted {
                        provider: PROVIDER.to_string(),
                        attempts: attempt + 1,
                        last_error: error.to_string(),
                    });
                }
                last_error = error.to_string();
            }
        }
    }

    Err(EmbedError::RetriesExhausted {
        provider: PROVIDER.to_string(),
        attempts: max_attempts,
        last_error,
    })
}

fn should_retry_status(policy: &RetryPolicy, status: u16) -> bool {
    status == 408 || policy.should_retry_status(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, CONTENT_TYPE};
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 2,
            initial_backoff: Duration::ZERO,
            request_timeout: Duration::from_secs(5),
            batch_size: 32,
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
            &test_policy(),
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
            &test_policy(),
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
            &test_policy(),
        )
        .await
        .expect_err("400 should fail without retrying");

        // Then: callers receive the provider status and response body.
        assert!(error.to_string().contains("HTTP 400: bad request"));
    }
}
