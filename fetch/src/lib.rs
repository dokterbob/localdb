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

    /// Test-only: the guarded **redirect policy** alone — default resolver, no
    /// preflight.
    ///
    /// Layer 3 is otherwise unreachable from a test. Every redirect fixture
    /// has to be a local server, and against `new_public_only()` the preflight
    /// (layer 2) or the guarded resolver (layer 1) refuses the *initial*
    /// request to that server, so the chain never starts and the redirect
    /// policy is never consulted. Disabling the two layers that guard the
    /// first hop is what lets the tests drive the third.
    #[cfg(test)]
    fn new_redirect_guard_only() -> Result<Self, Error> {
        let client = Self::builder()
            .redirect(destination::guarded_redirect_policy())
            .build()
            .map_err(|e| Error::ProviderUnavailable {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            policy: DestinationPolicy::Unrestricted,
        })
    }

    /// Settings shared by both constructors.
    fn builder() -> reqwest::ClientBuilder {
        Client::builder()
            .user_agent("localdb/0.1")
            .timeout(std::time::Duration::from_secs(30))
    }
}

/// Render a `reqwest::Error` together with its cause chain.
///
/// reqwest's own `Display` is deliberately terse and names only the outermost
/// layer — a redirect budget exhaustion prints as "error following redirect
/// for url (...)", with the actual reason buried in `source()`. Since this
/// string is all the operator ever sees (it becomes the `ProviderUnavailable`
/// message and lands in the run's error output), the chain is worth spelling
/// out.
fn describe_error(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        parts.push(e.to_string());
        source = e.source();
    }
    parts.join(": ")
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
                    message: format!("HTTP request failed: {}", describe_error(&e)),
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

    // -----------------------------------------------------------------------
    // Layer 3 — the guarded redirect policy
    //
    // Driven through `new_redirect_guard_only()`; see its doc comment for why
    // the other two layers have to be off for these to be reachable at all.
    // -----------------------------------------------------------------------

    /// The security-critical branch: a hop whose target is a blocked IP
    /// literal is refused, and the hop target is never requested.
    #[tokio::test]
    async fn guarded_redirect_refuses_a_hop_to_a_blocked_ip_literal() {
        let server = MockServer::start().await;
        // `server.uri()` is `http://127.0.0.1:<port>` — an IP literal, which
        // is exactly what this layer inspects.
        Mock::given(method("GET"))
            .and(path("/hop"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/internal", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secret"))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new_redirect_guard_only()
            .expect("new_redirect_guard_only should succeed in tests");
        let result = fetcher
            .fetch(&format!("{}/hop", server.uri()), &FetchMetadata::default())
            .await
            .expect("a blocked redirect target is Ok(Blocked), never Err");

        assert!(
            matches!(result, FetchResult::Blocked),
            "expected Blocked, got {result:?}"
        );
        let hits = server.received_requests().await.unwrap_or_default();
        assert!(
            hits.iter().all(|r| r.url.path() != "/internal"),
            "the redirect target must never be requested"
        );
    }

    /// The policy must not over-block: a hop to a *hostname* is followed
    /// normally (name targets are layer 1's job, not this layer's).
    #[tokio::test]
    async fn guarded_redirect_follows_a_hostname_hop() {
        let server = MockServer::start().await;
        let port = server.address().port();
        let target = format!("http://localhost:{port}/final");
        Mock::given(method("GET"))
            .and(path("/hop"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"followed"))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new_redirect_guard_only()
            .expect("new_redirect_guard_only should succeed in tests");
        let result = fetcher
            .fetch(
                &format!("http://localhost:{port}/hop"),
                &FetchMetadata::default(),
            )
            .await
            .unwrap();

        match result {
            FetchResult::Downloaded {
                bytes, final_url, ..
            } => {
                assert_eq!(bytes, b"followed");
                assert_eq!(final_url.as_deref(), Some(target.as_str()));
            }
            other => panic!("expected Downloaded, got {other:?}"),
        }
    }

    /// `Policy::custom` replaces reqwest's default outright, so the 10-hop cap
    /// is restated by hand — this pins that it actually terminates, and as an
    /// error rather than as a bare 30x handed back to the caller.
    #[tokio::test]
    async fn guarded_redirect_enforces_the_hop_cap() {
        let server = MockServer::start().await;
        let port = server.address().port();
        let loop_url = format!("http://localhost:{port}/loop");
        Mock::given(method("GET"))
            .and(path("/loop"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", loop_url.as_str()))
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new_redirect_guard_only()
            .expect("new_redirect_guard_only should succeed in tests");
        let result = fetcher.fetch(&loop_url, &FetchMetadata::default()).await;

        match result {
            Err(Error::ProviderUnavailable { message }) => assert!(
                message.contains("too many redirects"),
                "the cap must report itself as a redirect budget exhaustion, \
                 not as a bare 30x status: {message}"
            ),
            other => panic!("expected ProviderUnavailable, got {other:?}"),
        }
        // Exhausting the budget says nothing about the destination, so it must
        // NOT be laundered into the stable `Blocked` outcome.
        assert!(
            server.received_requests().await.unwrap_or_default().len() <= MAX_HOPS_SANITY,
            "the redirect loop must terminate, not spin"
        );
    }

    /// Generous upper bound for the hop-cap test — the point is "terminates",
    /// not an exact count (reqwest's bookkeeping of `previous` is its own).
    const MAX_HOPS_SANITY: usize = 20;

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
