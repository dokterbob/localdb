mod destination;

use std::sync::Arc;

use async_trait::async_trait;
use localdb_core::{
    error::Error,
    ingestion::{FetchMetadata, FetchResult, UrlFetcher},
};
use reqwest::{Client, StatusCode};

/// Which destinations a fetcher is willing to connect to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationPolicy {
    /// Anything the operator points us at, including loopback and private
    /// ranges. Correct for operator-configured locators (`url` sources, a
    /// feed's own URL): a homelab or LAN address is a legitimate choice
    /// there, and refusing it would break a real use case to guard against
    /// nothing — the operator already chose the target.
    Unrestricted,
    /// Globally-routable destinations only. For locators chosen by a third
    /// party — today, the `<link>` of a feed entry. See [`destination`].
    PublicOnly,
}

/// HTTP URL fetcher backed by reqwest.
///
/// `Clone` (cheap: `reqwest::Client` is internally `Arc`-backed) so callers
/// can build one client per run and hand each URL-kind source its own boxed
/// instance without rebuilding the underlying HTTP client per source.
#[derive(Clone)]
pub struct HttpUrlFetcher {
    client: Client,
    policy: DestinationPolicy,
}

impl HttpUrlFetcher {
    /// A fetcher with no destination restrictions — for operator-configured
    /// URLs. See [`DestinationPolicy::Unrestricted`].
    pub fn new() -> Result<Self, Error> {
        let client = Self::builder()
            .build()
            .map_err(|e| Error::ProviderUnavailable {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            policy: DestinationPolicy::Unrestricted,
        })
    }

    /// A fetcher that refuses any destination which is not globally routable,
    /// on the initial request and on every redirect hop.
    ///
    /// Use this for locators that came from untrusted content. A refusal is
    /// reported as `Ok(FetchResult::Blocked)`, never as an error: it is a
    /// stable, unambiguous outcome (it will be refused again next run), so it
    /// belongs beside `Gone` rather than in the transient-failure bucket.
    pub fn new_public_only() -> Result<Self, Error> {
        let client = Self::builder()
            .dns_resolver(Arc::new(destination::GuardedResolver))
            .redirect(destination::guarded_redirect_policy())
            .build()
            .map_err(|e| Error::ProviderUnavailable {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            policy: DestinationPolicy::PublicOnly,
        })
    }

    /// Settings shared by both constructors.
    fn builder() -> reqwest::ClientBuilder {
        Client::builder()
            .user_agent("localdb/0.1")
            .timeout(std::time::Duration::from_secs(30))
    }
}

#[async_trait]
impl UrlFetcher for HttpUrlFetcher {
    async fn fetch(&self, url: &str, metadata: &FetchMetadata) -> Result<FetchResult, Error> {
        // Preflight (destination guard layer 2). Mandatory for IP literals:
        // hyper-util's connector parses the host as a socket address before it
        // ever consults a custom DNS resolver, so `http://127.0.0.1/` would
        // otherwise never reach `GuardedResolver`. A URL that does not parse
        // is left alone — `send()` below reports it with a better message.
        if self.policy == DestinationPolicy::PublicOnly {
            if let Ok(parsed) = reqwest::Url::parse(url) {
                if destination::ip_literal_host(&parsed)
                    .is_some_and(destination::is_blocked_destination)
                {
                    tracing::info!(url = %url, "fetch: destination blocked (non-routable IP literal)");
                    return Ok(FetchResult::Blocked);
                }
            }
        }

        let mut req = self.client.get(url);

        if let Some(etag) = &metadata.etag {
            req = req.header("If-None-Match", etag);
        }
        if let Some(last_modified) = &metadata.last_modified {
            req = req.header("If-Modified-Since", last_modified);
        }

        let response = match req.send().await {
            Ok(response) => response,
            Err(e) => {
                // A rejection from the guarded resolver (layer 1) or the
                // guarded redirect policy (layer 3) reaches us only as an
                // opaque `reqwest::Error`; recover it so the caller sees the
                // stable `Blocked` outcome rather than a transient failure.
                if destination::is_blocked_error(&e) {
                    tracing::info!(url = %url, "fetch: destination blocked ({e})");
                    return Ok(FetchResult::Blocked);
                }
                return Err(Error::ProviderUnavailable {
                    message: format!("HTTP request failed: {e}"),
                });
            }
        };

        let status = response.status();

        if status == StatusCode::NOT_MODIFIED {
            return Ok(FetchResult::NotModified);
        }

        if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            return Ok(FetchResult::Gone);
        }

        if !status.is_success() {
            return Err(Error::ProviderUnavailable {
                message: format!("HTTP error {status} fetching {url}"),
            });
        }

        // Captured before `response.bytes()` consumes the response below:
        // `Response::url()` is the effective URL after any redirects reqwest
        // followed (reqwest 0.12's default `Policy::limited(10)`, which this
        // client's builder never overrides).
        let final_url = response.url().to_string();

        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let bytes = response
            .bytes()
            .await
            .map_err(|e| Error::ProviderUnavailable {
                message: format!("Failed to read response body: {e}"),
            })?
            .to_vec();

        Ok(FetchResult::Downloaded {
            bytes,
            content_type,
            etag,
            last_modified,
            final_url: Some(final_url),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{header, header_exists, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[test]
    fn http_url_fetcher_new_returns_err() {
        let result = HttpUrlFetcher::new();
        assert!(
            result.is_ok(),
            "HttpUrlFetcher::new() should return Ok in normal conditions"
        );
    }

    #[tokio::test]
    async fn test_200_with_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"hello world")
                    .insert_header("etag", "\"abc123\"")
                    .insert_header("last-modified", "Wed, 21 Oct 2025 07:28:00 GMT")
                    .insert_header("content-type", "text/plain"),
            )
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let requested_url = format!("{}/doc", server.uri());
        let result = fetcher
            .fetch(&requested_url, &FetchMetadata::default())
            .await
            .unwrap();

        match result {
            FetchResult::Downloaded {
                bytes,
                content_type,
                etag,
                last_modified,
                final_url,
            } => {
                assert_eq!(bytes, b"hello world");
                assert_eq!(content_type.as_deref(), Some("text/plain"));
                assert_eq!(etag.as_deref(), Some("\"abc123\""));
                assert_eq!(
                    last_modified.as_deref(),
                    Some("Wed, 21 Oct 2025 07:28:00 GMT")
                );
                assert_eq!(
                    final_url.as_deref(),
                    Some(requested_url.as_str()),
                    "no redirect happened, so final_url must equal the requested URL"
                );
            }
            other => panic!("expected Downloaded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_redirect_reports_final_url_as_redirect_target() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/old"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/new", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/new"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"redirected body"))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let requested_url = format!("{}/old", server.uri());
        let expected_final_url = format!("{}/new", server.uri());
        let result = fetcher
            .fetch(&requested_url, &FetchMetadata::default())
            .await
            .unwrap();

        match result {
            FetchResult::Downloaded {
                bytes, final_url, ..
            } => {
                assert_eq!(bytes, b"redirected body");
                assert_eq!(
                    final_url.as_deref(),
                    Some(expected_final_url.as_str()),
                    "final_url must be the redirect TARGET, not the originally requested URL"
                );
            }
            other => panic!("expected Downloaded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_304_not_modified() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .and(header("If-None-Match", "\"abc123\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let meta = FetchMetadata {
            etag: Some("\"abc123\"".to_string()),
            last_modified: None,
        };
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &meta)
            .await
            .unwrap();

        assert!(matches!(result, FetchResult::NotModified));
    }

    #[tokio::test]
    async fn test_if_none_match_header_sent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .and(header_exists("If-None-Match"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let meta = FetchMetadata {
            etag: Some("\"etag-value\"".to_string()),
            last_modified: None,
        };
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &meta)
            .await
            .unwrap();

        assert!(matches!(result, FetchResult::NotModified));
    }

    #[tokio::test]
    async fn test_404_gone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .unwrap();

        assert!(matches!(result, FetchResult::Gone));
    }

    #[tokio::test]
    async fn test_410_gone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .unwrap();

        assert!(matches!(result, FetchResult::Gone));
    }

    #[tokio::test]
    async fn test_500_provider_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await;

        assert!(matches!(result, Err(Error::ProviderUnavailable { .. })));
    }

    #[tokio::test]
    async fn test_connection_refused_provider_unavailable() {
        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let result = fetcher
            .fetch("http://127.0.0.1:1", &FetchMetadata::default())
            .await;

        assert!(matches!(result, Err(Error::ProviderUnavailable { .. })));
    }

    // -----------------------------------------------------------------------
    // Destination guard (`new_public_only`)
    //
    // Every test above uses `new()` deliberately: wiremock binds loopback,
    // which the guard blocks — which is precisely why the guard is opt-in via
    // a second constructor rather than applied to the existing client.
    // -----------------------------------------------------------------------

    /// The one test where wiremock's loopback binding is the *asset*: it gives
    /// us a live server we can prove was never contacted. Asserting zero
    /// received requests is what distinguishes "refused before connecting"
    /// from "connected, then classified the response as blocked".
    #[tokio::test]
    async fn public_only_refuses_loopback_without_connecting() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secret"))
            .mount(&server)
            .await;

        let fetcher =
            HttpUrlFetcher::new_public_only().expect("new_public_only should succeed in tests");
        let result = fetcher
            .fetch(
                &format!("{}/internal", server.uri()),
                &FetchMetadata::default(),
            )
            .await
            .expect("a blocked destination is Ok(Blocked), never Err");

        assert!(
            matches!(result, FetchResult::Blocked),
            "expected Blocked, got {result:?}"
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the guard must refuse before any connection is made"
        );
    }

    /// Layer 2 in isolation: an obfuscated decimal IP literal. `Url::parse`
    /// normalizes it to 127.0.0.1 before the guard ever looks, which is why
    /// the check goes through `reqwest::Url` rather than the raw string.
    #[tokio::test]
    async fn public_only_refuses_obfuscated_ip_literal() {
        let fetcher =
            HttpUrlFetcher::new_public_only().expect("new_public_only should succeed in tests");
        let result = fetcher
            .fetch("http://2130706433/", &FetchMetadata::default())
            .await
            .expect("a blocked destination is Ok(Blocked), never Err");
        assert!(matches!(result, FetchResult::Blocked));
    }

    /// Layer 1: a *name* that resolves to loopback never reaches the preflight
    /// (it is not an IP literal), so this exercises `GuardedResolver` and the
    /// `reqwest::Error` → `Blocked` recovery walk end to end.
    #[tokio::test]
    async fn public_only_refuses_a_name_that_resolves_to_loopback() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secret"))
            .mount(&server)
            .await;

        let port = server.address().port();
        let fetcher =
            HttpUrlFetcher::new_public_only().expect("new_public_only should succeed in tests");
        let result = fetcher
            .fetch(
                &format!("http://localhost:{port}/internal"),
                &FetchMetadata::default(),
            )
            .await
            .expect("a resolver rejection must surface as Ok(Blocked), not Err");

        assert!(
            matches!(result, FetchResult::Blocked),
            "expected Blocked, got {result:?} — if this regresses to \
             Err(ProviderUnavailable), reqwest stopped preserving the error \
             source chain that `destination::is_blocked_error` walks. Security \
             is unaffected (the connection still never happens); the feed \
             ingestor just loses its fall-back-to-summary behavior."
        );
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "the guarded resolver must refuse before any connection is made"
        );
    }

    /// The unrestricted client is unchanged — it must still reach loopback,
    /// because operator-configured `url` sources and feed URLs legitimately
    /// point at LAN and homelab addresses.
    #[tokio::test]
    async fn unrestricted_fetcher_still_reaches_loopback() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"local content"))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new().expect("HttpUrlFetcher::new should succeed in tests");
        let result = fetcher
            .fetch(
                &format!("{}/internal", server.uri()),
                &FetchMetadata::default(),
            )
            .await
            .unwrap();

        assert!(matches!(result, FetchResult::Downloaded { .. }));
    }
}
