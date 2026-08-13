mod destination;
pub mod http;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use backon::Retryable;
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
/// `Clone` (cheap: `reqwest::Client` and `http::HostLimiter` are both
/// internally `Arc`-backed) so callers can build one client per run and hand
/// each URL-kind source its own boxed instance without rebuilding the
/// underlying HTTP client or losing track of per-host pacing state.
#[derive(Clone)]
pub struct HttpUrlFetcher {
    client: Client,
    policy: DestinationPolicy,
    /// Retry-schedule knobs (`max_retries`) applied per fetch. `HostLimiter`
    /// is the other half of `http::HttpSettings` and lives in its own field
    /// below rather than being re-derived from this one, because `new_pair`
    /// needs the *same* `HostLimiter` instance shared between two
    /// `HttpUrlFetcher`s while each still carries its own copy of the plain
    /// settings.
    settings: http::HttpSettings,
    /// Per-host request pacing (issue #207). Shared between the pair
    /// returned by [`Self::new_pair`] so pacing is per-host across the whole
    /// run rather than per-fetcher; `new()`/`new_public_only()` each get
    /// their own, since nothing else in this crate ever needs those two to
    /// share one.
    limiter: http::HostLimiter,
}

impl HttpUrlFetcher {
    /// A fetcher with no destination restrictions — for operator-configured
    /// URLs. See [`DestinationPolicy::Unrestricted`].
    ///
    /// Uses `http::HttpSettings::default()` and a limiter of its own. Callers
    /// that have an operator-configured `http:` section and want its retry
    /// and rate-limit knobs applied — and, in particular, callers that also
    /// build a [`Self::new_public_only`] fetcher for the same run — should
    /// use [`Self::new_pair`] instead so both fetchers share one
    /// `HostLimiter`.
    pub fn new() -> Result<Self, Error> {
        let settings = http::HttpSettings::default();
        let limiter = http::HostLimiter::new(&settings);
        Self::build(DestinationPolicy::Unrestricted, settings, limiter)
    }

    /// A fetcher that refuses any destination which is not globally routable,
    /// on the initial request and on every redirect hop.
    ///
    /// Use this for locators that came from untrusted content. A refusal is
    /// reported as `Ok(FetchResult::Blocked)`, never as an error: it is a
    /// stable, unambiguous outcome (it will be refused again next run), so it
    /// belongs beside `Gone` rather than in the transient-failure bucket.
    ///
    /// Uses `http::HttpSettings::default()` and a limiter of its own — see
    /// [`Self::new`]'s doc comment for when [`Self::new_pair`] is the better
    /// choice.
    pub fn new_public_only() -> Result<Self, Error> {
        let settings = http::HttpSettings::default();
        let limiter = http::HostLimiter::new(&settings);
        Self::build(DestinationPolicy::PublicOnly, settings, limiter)
    }

    /// Build both fetchers a run typically needs — one unrestricted, one
    /// public-only — sharing a single [`http::HostLimiter`] and the same
    /// operator-configured `cfg`.
    ///
    /// Sharing the limiter is the point: without it, an unrestricted fetch of
    /// the operator's own feed URL and a public-only fetch of that feed's
    /// entry links would pace independently against the same origin,
    /// defeating the purpose of per-host pacing (issue #207) whenever a run
    /// touches one host through both fetchers. Returns `(unrestricted,
    /// public_only)`.
    pub fn new_pair(cfg: &http::HttpSettings) -> Result<(Self, Self), Error> {
        let limiter = http::HostLimiter::new(cfg);
        let unrestricted = Self::build(
            DestinationPolicy::Unrestricted,
            cfg.clone(),
            limiter.clone(),
        )?;
        let public_only = Self::build(DestinationPolicy::PublicOnly, cfg.clone(), limiter)?;
        Ok((unrestricted, public_only))
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
        Self::new_redirect_guard_only_with(&http::HttpSettings::default())
    }

    /// Same as [`Self::new_redirect_guard_only`], but with injectable
    /// settings — for a test that needs the guarded redirect policy *and*
    /// `fast_settings`' fast retry timing (e.g. a redirect-then-429 test
    /// that has no interest in exercising the honored-`Retry-After` sleep
    /// itself, only where its cooldown gets recorded).
    #[cfg(test)]
    fn new_redirect_guard_only_with(settings: &http::HttpSettings) -> Result<Self, Error> {
        let limiter = http::HostLimiter::new(settings);
        let client = http::client_builder(settings)
            .redirect(destination::guarded_redirect_policy())
            .build()
            .map_err(|e| Error::ProviderUnavailable {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            policy: DestinationPolicy::Unrestricted,
            settings: settings.clone(),
            limiter,
        })
    }

    /// Shared client construction for every constructor above.
    ///
    /// All destination-policy-independent settings (user agent, timeout) come
    /// from `http::client_builder`; the two destination-guard layers that
    /// only `PublicOnly` applies (`GuardedResolver`, the guarded redirect
    /// policy) are layered on here, exactly as the pre-#207 `builder()`
    /// helper did.
    fn build(
        policy: DestinationPolicy,
        settings: http::HttpSettings,
        limiter: http::HostLimiter,
    ) -> Result<Self, Error> {
        let mut builder = http::client_builder(&settings);
        if policy == DestinationPolicy::PublicOnly {
            builder = builder
                .dns_resolver(Arc::new(destination::GuardedResolver))
                .redirect(destination::guarded_redirect_policy());
        }
        let client = builder.build().map_err(|e| Error::ProviderUnavailable {
            message: format!("failed to build HTTP client: {e}"),
        })?;
        Ok(Self {
            client,
            policy,
            settings,
            limiter,
        })
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
    /// Fetch `url`, retrying transient failures and pacing per-host request
    /// rate along the way (issue #207).
    ///
    /// Structure, in order:
    ///
    /// 1. The preflight IP-literal check for `PublicOnly` — unchanged from
    ///    before this stage, and deliberately *outside* the retried block: a
    ///    blocked destination is refused once, immediately, never retried.
    /// 2. A single `limiter.acquire()` before the retried block, not inside
    ///    it — pacing applies once per `fetch()` call. A request that then
    ///    goes on to retry several times already paid its host's pacing cost
    ///    up front; re-acquiring per attempt would make a single slow
    ///    document pay pacing tax multiple times for no benefit, since the
    ///    thing pacing protects against (origin-visible request rate) is
    ///    already satisfied by the one wait.
    /// 3. The retried block: send, classify the outcome, and — for a
    ///    success — read the body. The body read happens *inside* the
    ///    closure so a connection that fails partway through a body is
    ///    retried too, not just a failed `send()`.
    /// 4. Terminal mapping from the retry loop's `Result` to this method's
    ///    `Result<FetchResult, Error>`.
    ///
    /// See `http::retry`'s module doc for the closure contract this method
    /// implements, and its own doc comments below for the per-branch
    /// reasoning.
    async fn fetch(&self, url: &str, metadata: &FetchMetadata) -> Result<FetchResult, Error> {
        // Preflight (destination guard layer 2). Mandatory for IP literals:
        // hyper-util's connector parses the host as a socket address before it
        // ever consults a custom DNS resolver, so `http://127.0.0.1/` would
        // otherwise never reach `GuardedResolver`. A URL that does not parse
        // is left alone — `send()` below reports it with a better message.
        //
        // Parsed once here and reused for pacing/host-keying below, rather
        // than re-parsing at each use site.
        let parsed_url = reqwest::Url::parse(url).ok();

        if self.policy == DestinationPolicy::PublicOnly {
            if let Some(parsed) = &parsed_url {
                if destination::ip_literal_host(parsed)
                    .is_some_and(destination::is_blocked_destination)
                {
                    tracing::info!(url = %url, "fetch: destination blocked (non-routable IP literal)");
                    return Ok(FetchResult::Blocked);
                }
            }
        }

        // Proactive pacing (issue #207): wait for this host's token bucket
        // and any active `Retry-After` cooldown *before* making the first
        // attempt. A no-op for loopback/LAN destinations and for a URL with
        // no host at all — see `http::limiter::should_pace`.
        if let Some(parsed) = &parsed_url {
            self.limiter.acquire(parsed).await;
        }

        // Lowercased once, matching `HostLimiter`'s own key normalization —
        // this is the *requested* URL's host, used to name the host in a
        // `RateLimited` message (`map_outcome`, below). Deliberately not
        // used to record a 429's `Retry-After` cooldown — that uses the
        // *effective* (post-redirect) host instead, computed from the
        // response itself once one arrives; see `classify_response`'s doc
        // comment for why the two must differ on a cross-host redirect.
        let host = parsed_url
            .as_ref()
            .and_then(|u| u.host_str())
            .map(str::to_lowercase);

        let attempt = || async {
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
                    // opaque `reqwest::Error`; recover it so the caller sees
                    // the stable `Blocked` outcome rather than a transient
                    // failure. Returned as `Ok`, not `Err(RetryError::
                    // Request(e))`, so it never even reaches `is_transient`
                    // below — a blocked destination must never be retried,
                    // even though it surfaces in the same connect-error shape
                    // as an ordinary transient network failure (the trap
                    // `http::retry::is_transient`'s doc comment documents at
                    // length).
                    if destination::is_blocked_error(&e) {
                        tracing::info!(url = %url, "fetch: destination blocked ({e})");
                        return Ok(FetchResult::Blocked);
                    }
                    return Err(http::RetryError::Request(e));
                }
            };

            self.classify_response(response).await
        };

        let outcome = attempt
            .retry(http::retry_policy(&self.settings))
            .when(http::is_transient)
            // Honor a `Retry-After` hint (capped at `INLINE_RETRY_AFTER_CAP`,
            // and tracked against this fetch's own cumulative-sleep budget
            // independently of backon's internal accounting) or fall back to
            // backon's own computed delay when there is no hint. See
            // `http::retry_after_adjuster`'s doc comment for the full
            // contract, including why the `dur?` short-circuit inside it is
            // load-bearing.
            .adjust(http::retry_after_adjuster(http::total_retry_budget(
                &self.settings,
            )))
            // The only signal of retry activity: progress events are emitted
            // after `fetch()` returns, so the CLI/daemon progress bar shows
            // nothing while a document is being retried.
            .notify(|err: &http::RetryError, dur: Duration| {
                tracing::warn!(url = %url, wait = ?dur, error = ?err, "fetch: retrying after a transient failure");
            })
            .await;

        Self::map_outcome(url, &host, outcome)
    }
}

impl HttpUrlFetcher {
    /// Turn a response that actually arrived into a `FetchResult` or a
    /// retryable/fatal `RetryError` — the second half of `fetch`'s retried
    /// closure, split out to shrink `fetch`'s own branch count.
    ///
    /// Does **not** participate in destination-guard ordering: by the time
    /// this is called, `req.send()` already returned `Ok`, so the guard
    /// refusal path (`destination::is_blocked_error`, checked on a `send()`
    /// *error* in the closure above) has already been ruled out by the
    /// caller. This function only ever sees a response that was actually
    /// received.
    ///
    /// Takes no `host` parameter, deliberately — unlike `fetch`'s pacing
    /// `acquire()` call, which necessarily runs *before* the request exists
    /// and so can only ever key off the *requested* URL's host, everything
    /// this method does with a host (recording a 429's cooldown below)
    /// happens *after* a response has actually arrived, so it uses that
    /// response's own effective (post-redirect) host instead. See
    /// `effective_host` below.
    async fn classify_response(
        &self,
        response: reqwest::Response,
    ) -> Result<FetchResult, http::RetryError> {
        let status = response.status();

        if status == StatusCode::NOT_MODIFIED {
            return Ok(FetchResult::NotModified);
        }

        if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
            return Ok(FetchResult::Gone);
        }

        // `Response::url()` is the effective URL after any redirects reqwest
        // followed (reqwest 0.12's default `Policy::limited(10)`, which this
        // client's builder never overrides) — read here, before
        // `response.bytes()` consumes the response further down, and reused
        // both for `final_url` on success and for cooldown attribution on a
        // 429 below. Deliberately *not* the `host` computed once in `fetch`
        // from the originally *requested* URL: on a cross-host redirect
        // (the redirector 30x's to a different host, and *that* host is the
        // one that actually returns 429), attributing the cooldown to the
        // requested host would record it against the redirector — a host
        // that was never the one imposing the limit — leaving the real
        // limiting host's cooldown unset, so later requests straight to it
        // (or via a different redirector) would sail through.
        let effective_url = response.url().clone();
        let effective_host = effective_url.host_str().map(str::to_lowercase);

        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(http::parse_retry_after);

            // Record every 429's `Retry-After` against the *effective*
            // host's cooldown, not only the ones too large to sleep on
            // inline below (`http::INLINE_RETRY_AFTER_CAP`) — this is what
            // actually holds back *other* requests already in flight or
            // about to be sent to the same host for the rest of the run,
            // which is the mechanism issue #207's 23-req/s log needs.
            if status == StatusCode::TOO_MANY_REQUESTS {
                if let (Some(host), Some(retry_after)) = (&effective_host, retry_after) {
                    self.limiter.note_retry_after(host, retry_after);
                }
            }

            return Err(http::RetryError::Status {
                status,
                retry_after,
            });
        }

        let final_url = effective_url.to_string();

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

        // Read inside the closure, deliberately: a body that fails to
        // read to completion (a truncated connection mid-transfer) must
        // be retried like any other transient failure, not surfaced as a
        // one-shot error the way it would be if this read happened after
        // `.retry()` returned.
        let bytes = response
            .bytes()
            .await
            .map_err(http::RetryError::Request)?
            .to_vec();

        Ok(FetchResult::Downloaded {
            bytes,
            content_type,
            etag,
            last_modified,
            final_url: Some(final_url),
        })
    }

    /// Map the retry loop's terminal `Result` to `fetch`'s own return type —
    /// the fourth step described in `fetch`'s own doc comment. Split out
    /// purely to shrink `fetch`'s branch count; this runs only after the
    /// retry loop (and every destination-guard check inside it) has already
    /// finished, so it has no bearing on guard ordering.
    fn map_outcome(
        url: &str,
        host: &Option<String>,
        outcome: Result<FetchResult, http::RetryError>,
    ) -> Result<FetchResult, Error> {
        match outcome {
            Ok(result) => Ok(result),
            Err(http::RetryError::Status {
                status,
                retry_after,
            }) if status == StatusCode::TOO_MANY_REQUESTS => {
                // The one outcome that is not `ProviderUnavailable`: retries
                // on a 429 were exhausted (the total retry budget, the max
                // attempt count, or the server's own `Retry-After` was too
                // large to wait on inline). Distinct from every other
                // exhausted-retry outcome so a caller — and an operator
                // reading logs — can tell "the server is overloaded" apart
                // from "the server is broken", per issue #207.
                let host_desc = host.as_deref().unwrap_or("unknown host");
                let retry_after_desc = retry_after
                    .map(|d| format!(", last Retry-After: {}s", d.as_secs()))
                    .unwrap_or_default();
                Err(Error::RateLimited {
                    message: format!(
                        "fetching {url} (host {host_desc}); retries exhausted{retry_after_desc}"
                    ),
                })
            }
            Err(http::RetryError::Status { status, .. }) => Err(Error::ProviderUnavailable {
                message: format!("HTTP error {status} fetching {url}"),
            }),
            Err(http::RetryError::Request(e)) => Err(Error::ProviderUnavailable {
                message: format!("HTTP request failed: {}", describe_error(&e)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use wiremock::{
        matchers::{header, header_exists, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    /// Settings for tests that need to exercise the retry loop without
    /// paying the production 1s-minimum exponential backoff for real:
    /// `min_retry_delay` is dialed down to 1ms, which `http::retry_policy`
    /// also scales `max_delay`/`total_delay` down with (see its doc
    /// comment), so a computed (non-`Retry-After`-driven) backoff never adds
    /// more than a few milliseconds of real sleep no matter how many retries
    /// a test forces. A test that wants to prove a server's actual
    /// `Retry-After` value is honored (e.g. `test_retry_after_seconds_is_
    /// honored`) still gets a real wait despite this — that value overrides
    /// the computed backoff entirely in `fetch`'s `.adjust` closure,
    /// independent of `min_retry_delay`.
    ///
    /// Rate limiting is left at the default — every wiremock fixture in this
    /// module binds `127.0.0.1`, which `http::limiter::should_pace` exempts
    /// unconditionally, so it never adds latency here regardless of the
    /// configured rate.
    fn fast_settings(max_retries: u32) -> http::HttpSettings {
        http::HttpSettings {
            max_retries,
            min_retry_delay: Duration::from_millis(1),
            ..http::HttpSettings::default()
        }
    }

    /// An unrestricted fetcher built through `new_pair` so tests can inject
    /// `fast_settings` — `new()` itself is fixed to `HttpSettings::default()`
    /// (3 retries), too slow for a test that wants to force retries.
    fn unrestricted_with(cfg: &http::HttpSettings) -> HttpUrlFetcher {
        HttpUrlFetcher::new_pair(cfg)
            .expect("new_pair should succeed in tests")
            .0
    }

    /// The public-only half of the same pairing — see `unrestricted_with`.
    fn public_only_with(cfg: &http::HttpSettings) -> HttpUrlFetcher {
        HttpUrlFetcher::new_pair(cfg)
            .expect("new_pair should succeed in tests")
            .1
    }

    #[test]
    fn http_url_fetcher_new_returns_err() {
        let result = HttpUrlFetcher::new();
        assert!(
            result.is_ok(),
            "HttpUrlFetcher::new() should return Ok in normal conditions"
        );
    }

    #[tokio::test]
    async fn new_pair_builds_a_working_unrestricted_and_public_only_fetcher() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok"))
            .mount(&server)
            .await;

        let (unrestricted, public_only) = HttpUrlFetcher::new_pair(&http::HttpSettings::default())
            .expect("new_pair should succeed in tests");

        let ok = unrestricted
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .unwrap();
        assert!(matches!(ok, FetchResult::Downloaded { .. }));

        let blocked = public_only
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .expect("a blocked destination is Ok(Blocked), never Err");
        assert!(matches!(blocked, FetchResult::Blocked));
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

    // -----------------------------------------------------------------------
    // Retry (issue #207): transient status codes get retried, fatal ones
    // don't, and a 429 that never recovers becomes `Error::RateLimited`.
    //
    // Every test here uses `fast_settings` — see its doc comment for why.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_429_then_200_is_retried_and_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok"))
            .expect(1)
            .mount(&server)
            .await;

        let fetcher = unrestricted_with(&fast_settings(2));
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .expect("a 429 followed by a 200 must eventually succeed");

        assert!(matches!(result, FetchResult::Downloaded { .. }));
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            2,
            "exactly 2 requests must have reached the server: the initial 429 and one retry"
        );
    }

    #[tokio::test]
    async fn test_429_exhausted_returns_rate_limited() {
        let server = MockServer::start().await;
        // max_retries=1 below means 2 total attempts; `.expect(2)` pins that
        // exactly, both against this one always-429 mock.
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .expect(2)
            .mount(&server)
            .await;

        // Not `fast_settings`: this test injects a real 1s `Retry-After`,
        // which the retry-budget adjuster (`http::retry_after_adjuster`)
        // must actually be able to afford for the one retry this test
        // expects to happen. `fast_settings`' 1ms floor scales the total
        // budget (`min_retry_delay` × 30, see `http::total_retry_budget`)
        // down to ~30ms — far short of the 1s this test injects — so a
        // correct (strict, pre-add) budget check would refuse that first
        // wait outright and the loop would stop after 1 request instead of
        // the 2 asserted below. 40ms gives a 1.2s budget, comfortably above
        // the 1s injected.
        let settings = http::HttpSettings {
            max_retries: 1,
            min_retry_delay: Duration::from_millis(40),
            ..http::HttpSettings::default()
        };
        let fetcher = unrestricted_with(&settings);
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await;

        match result {
            err @ Err(Error::RateLimited { .. }) => {
                // Assert on the *rendered* error, not the bare `message`
                // field: `Display` is what an operator actually reads, and
                // `Error::RateLimited`'s `#[error("rate limited: {message}")]`
                // already supplies that label. The match arm above is what
                // pins the variant; there is nothing left for the message
                // itself to prove except the context it adds.
                let rendered = err.unwrap_err().to_string();
                assert!(
                    rendered.contains("127.0.0.1"),
                    "rendered error should name the host: {rendered}"
                );
                // Regression: the message used to begin "rate limited
                // fetching ...", which `Display` then prefixed again —
                // operators saw "rate limited: rate limited fetching ...".
                // The label belongs to `Display` alone, so it must appear
                // exactly once.
                assert_eq!(
                    rendered.matches("rate limited").count(),
                    1,
                    "the 'rate limited' label must not be duplicated between \
                     Display's prefix and the message: {rendered}"
                );
            }
            other => panic!("expected Err(RateLimited), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_5xx_then_200_is_retried_and_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok"))
            .expect(1)
            .mount(&server)
            .await;

        let fetcher = unrestricted_with(&fast_settings(2));
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .expect("a 503 followed by a 200 must eventually succeed");

        assert!(matches!(result, FetchResult::Downloaded { .. }));
    }

    #[tokio::test]
    async fn test_400_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;

        let fetcher = unrestricted_with(&fast_settings(3));
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await;

        assert!(matches!(result, Err(Error::ProviderUnavailable { .. })));
    }

    #[tokio::test]
    async fn test_403_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;

        let fetcher = unrestricted_with(&fast_settings(3));
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await;

        assert!(matches!(result, Err(Error::ProviderUnavailable { .. })));
    }

    #[tokio::test]
    async fn test_retry_after_seconds_is_honored() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok"))
            .expect(1)
            .mount(&server)
            .await;

        // Not `fast_settings`: see the comment on the equivalent settings in
        // `test_429_exhausted_returns_rate_limited` — this test injects a
        // real 1s `Retry-After`, which needs a budget (`min_retry_delay` ×
        // 30) of at least 1s to be honorable under the strict, pre-add
        // budget check. 40ms gives 1.2s of budget.
        let settings = http::HttpSettings {
            max_retries: 2,
            min_retry_delay: Duration::from_millis(40),
            ..http::HttpSettings::default()
        };
        let fetcher = unrestricted_with(&settings);
        let start = std::time::Instant::now();
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .expect("must eventually succeed");
        let elapsed = start.elapsed();

        assert!(matches!(result, FetchResult::Downloaded { .. }));
        assert!(
            elapsed >= Duration::from_millis(900),
            "a 1s Retry-After should be honored inline, waited only {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "wait must stay bounded, got {elapsed:?}"
        );
    }

    /// Narrow by design: this only needs to prove the HTTP-date form of
    /// `Retry-After` is plumbed end to end through `fetch()` (parsed,
    /// honored as an inline sleep) — the exhaustive parsing cases (delta-
    /// seconds, an HTTP date in the future, a date in the past collapsing to
    /// zero, garbage, the absurd-value cap) already live as free, non-sleeping
    /// unit tests in `http::retry`'s own test module.
    ///
    /// Targets ~3s out, not ~1s: `httpdate::fmt_http_date` formats to
    /// whole-second granularity, so a ~1s-out target can serialize to a
    /// timestamp that has *already* elapsed by the time the response reaches
    /// the client, and `parse_retry_after` correctly treats a past date as
    /// `Duration::ZERO` — that made this test flaky, not `fetch()` buggy. A
    /// wider target leaves room for that truncation while still keeping the
    /// real sleep this test incurs (the one deliberate exception to this
    /// module's "no real sleep" rule for a computed backoff) small.
    #[tokio::test]
    async fn test_retry_after_http_date_is_honored() {
        let server = MockServer::start().await;
        let target = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(3));
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", target.as_str()))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok"))
            .expect(1)
            .mount(&server)
            .await;

        // Not `fast_settings`: this test's `Retry-After` targets ~3s out
        // (see the doc comment above on why not ~1s), so — same reasoning
        // as `test_retry_after_seconds_is_honored` — the budget
        // (`min_retry_delay` × 30) needs to be at least 3s. 100ms gives
        // exactly 3s; the actual parsed `Retry-After` is always strictly
        // less than 3s (httpdate's whole-second truncation can only make
        // the formatted timestamp earlier than the true 3s-out target,
        // never later), so this budget always accommodates it with room to
        // spare.
        let settings = http::HttpSettings {
            max_retries: 2,
            min_retry_delay: Duration::from_millis(100),
            ..http::HttpSettings::default()
        };
        let fetcher = unrestricted_with(&settings);
        let start = std::time::Instant::now();
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .expect("must eventually succeed");
        let elapsed = start.elapsed();

        assert!(matches!(result, FetchResult::Downloaded { .. }));
        assert!(
            elapsed >= Duration::from_millis(1500),
            "a ~3s-out HTTP-date Retry-After should be honored inline (allowing for \
             httpdate's whole-second truncation), waited only {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(8),
            "wait must stay bounded, got {elapsed:?}"
        );
    }

    /// Defect 1 regression (issue #207 follow-up): a `Retry-After` over
    /// `http::INLINE_RETRY_AFTER_CAP` (30s) used to be clamped down to the
    /// cap and slept on inline anyway. It must instead end the fetch
    /// immediately as `RateLimited` — no inline sleep, and critically, no
    /// second request either (`.expect(1)` below fails the test if one goes
    /// out). `max_retries` is set generously (5) specifically to prove the
    /// loop stops because of the oversized `Retry-After`, not because
    /// retries were exhausted some other way.
    #[tokio::test]
    async fn test_retry_after_over_cap_ends_fetch_immediately_with_no_inline_sleep() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "40"))
            .expect(1)
            .mount(&server)
            .await;

        let fetcher = unrestricted_with(&fast_settings(5));
        let start = std::time::Instant::now();
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await;
        let elapsed = start.elapsed();

        assert!(
            matches!(result, Err(Error::RateLimited { .. })),
            "expected RateLimited, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "an over-cap Retry-After must never be slept on inline, took {elapsed:?}"
        );
    }

    /// Defect 2 regression (issue #207 follow-up): honoring a `Retry-After`
    /// on every retry used to escape `with_total_delay`'s budget entirely,
    /// because `backon` only charges *its own* proposed (tiny, at this
    /// scale) delay against that budget, never the substituted
    /// `Retry-After` value actually slept on. A server that keeps
    /// responding with the same `Retry-After` (1s — the finest granularity
    /// the header supports; delta-seconds only, see `parse_retry_after`)
    /// must still have its *cumulative* honored waits bounded by
    /// `http::total_retry_budget` for these settings: `min_retry_delay` 50ms
    /// × `TOTAL_DELAY_RATIO` 30 = 1.5s — small enough that a 3rd 1s wait
    /// (bringing the running total to 3s) is refused once the first two
    /// (totalling 2s) have already met-or-exceeded it.
    ///
    /// With the fix: attempt 1 fails (0s spent so far < 1.5s budget,
    /// honored, running total → 1s), attempt 2 fails (1s spent < 1.5s,
    /// honored, running total → 2s), attempt 3 fails (2s spent ≥ 1.5s
    /// budget — refused, loop ends). 3 requests, ~2s of real sleep.
    /// `max_retries` is set generously (10) specifically so the fix — not
    /// `max_retries` running out — is what stops the loop; pre-fix, this
    /// same scenario kept going well past 2s because `backon`'s own
    /// accounting of its tiny computed delays doesn't approach the 1.5s
    /// budget until several retries later.
    #[tokio::test]
    async fn test_retry_after_cumulative_sleep_is_bounded_by_total_budget() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .mount(&server)
            .await;

        let settings = http::HttpSettings {
            max_retries: 10,
            min_retry_delay: Duration::from_millis(50),
            ..http::HttpSettings::default()
        };
        let fetcher = unrestricted_with(&settings);
        let start = std::time::Instant::now();
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await;
        let elapsed = start.elapsed();
        let requests = server.received_requests().await.unwrap_or_default().len();

        assert!(
            matches!(result, Err(Error::RateLimited { .. })),
            "expected RateLimited, got {result:?}"
        );
        assert!(
            requests <= 4,
            "cumulative honored Retry-After sleep should stop the loop well \
             before max_retries (10) is reached; got {requests} requests"
        );
        assert!(
            elapsed < Duration::from_millis(2500),
            "cumulative sleep must stay near the ~1.5s budget, not run for \
             most of max_retries × 1s Retry-After; got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_500_provider_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/doc"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        // Pinned to 0 retries: this test asserts *classification* (a 5xx
        // becomes `ProviderUnavailable`), not retry behavior — `test_5xx_
        // then_200_is_retried_and_succeeds` above owns that. Without pinning
        // this to 0, a 5xx is now a retryable status and this would silently
        // become a multi-second retry test.
        let fetcher = unrestricted_with(&fast_settings(0));
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await;

        assert!(matches!(result, Err(Error::ProviderUnavailable { .. })));
    }

    #[tokio::test]
    async fn test_connection_refused_provider_unavailable() {
        // Pinned to 0 retries for the same reason as `test_500_provider_
        // unavailable` above: a connect failure is retryable now, and this
        // test's intent is narrow classification, not retry behavior.
        let fetcher = unrestricted_with(&fast_settings(0));
        let result = fetcher
            .fetch("http://127.0.0.1:1", &FetchMetadata::default())
            .await;

        assert!(matches!(result, Err(Error::ProviderUnavailable { .. })));
    }

    // -----------------------------------------------------------------------
    // Destination guard (`new_public_only`)
    //
    // Every test above uses `new()`/`unrestricted_with` deliberately:
    // wiremock binds loopback, which the guard blocks — which is precisely
    // why the guard is opt-in via a second constructor rather than applied
    // to the existing client.
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
            .expect(0)
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
    ///
    /// **The regression test this stage exists to add**: the guard's
    /// rejection reaches `send()` as an ordinary-looking connect error — the
    /// exact shape `http::retry::is_transient` would otherwise classify as
    /// transient (see its doc comment on the `is_connect()` trap). Built with
    /// `fast_settings(3)` (not the default 0) and a wall-clock assertion, not
    /// just the zero-requests one below: if the retry closure ever regressed
    /// to routing this through `Err(RetryError::Request(..))` instead of
    /// short-circuiting to `Ok(Blocked)`, the request count would *still*
    /// read zero (the guard still refuses every attempt before connecting),
    /// but the call would take 3 retries' worth of backoff — this is what
    /// actually catches that regression.
    #[tokio::test]
    async fn public_only_refuses_a_name_that_resolves_to_loopback() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secret"))
            .expect(0)
            .mount(&server)
            .await;

        let port = server.address().port();
        let fetcher = public_only_with(&fast_settings(3));
        let start = std::time::Instant::now();
        let result = fetcher
            .fetch(
                &format!("http://localhost:{port}/internal"),
                &FetchMetadata::default(),
            )
            .await
            .expect("a resolver rejection must surface as Ok(Blocked), not Err");
        let elapsed = start.elapsed();

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
        assert!(
            elapsed < Duration::from_millis(500),
            "a blocked destination must never be retried — with 3 retries \
             configured, a naive is_connect()-based classifier would have \
             burned several seconds of backoff here; got {elapsed:?}"
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
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/internal"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"secret"))
            .expect(0)
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
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"followed"))
            .expect(1)
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

    /// Finding C regression: a 429's `Retry-After` cooldown must be recorded
    /// against the host that actually sent the 429 (the *effective*,
    /// post-redirect host), not the redirector's host — even though `host`
    /// in `fetch()` is computed once, before the request, from the
    /// *requested* URL. `/hop` on the `127.0.0.1` literal 302s to `/final`
    /// on the `localhost` name (both routed to the same wiremock server —
    /// see `guarded_redirect_follows_a_hostname_hop` above for why this pair
    /// of hostnames is what lets a test drive a real cross-host redirect
    /// against one loopback listener), and `/final` returns 429.
    ///
    /// Asserted on the limiter directly (`cooldown_is_set_for_test`), not
    /// through observable timing: both `127.0.0.1` and `localhost` are
    /// loopback-exempt from `acquire`'s pacing wait (`should_pace`), so
    /// there is no wait to time either way.
    ///
    /// `fast_settings(0)` (via `new_redirect_guard_only_with`), not
    /// `new_redirect_guard_only`'s production defaults: cooldown recording
    /// happens on every attempt regardless of whether a retry follows (see
    /// `classify_response`), so this test needs exactly one 429 response,
    /// not a real honored-`Retry-After` sleep — `max_retries: 0` gets that
    /// in one request instead of the 4 (and ~15s of real 5s sleeps) that
    /// production's `max_retries: 3` would otherwise cause here.
    #[tokio::test]
    async fn cross_host_redirect_records_cooldown_against_the_final_host_not_the_redirector() {
        let server = MockServer::start().await;
        let port = server.address().port();
        let target = format!("http://localhost:{port}/final");
        Mock::given(method("GET"))
            .and(path("/hop"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", target.as_str()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "5"))
            .expect(1)
            .mount(&server)
            .await;

        let fetcher = HttpUrlFetcher::new_redirect_guard_only_with(&fast_settings(0))
            .expect("new_redirect_guard_only_with should succeed in tests");
        let result = fetcher
            .fetch(
                &format!("http://127.0.0.1:{port}/hop"),
                &FetchMetadata::default(),
            )
            .await;

        assert!(
            matches!(result, Err(Error::RateLimited { .. })),
            "expected RateLimited, got {result:?}"
        );
        assert!(
            fetcher.limiter.cooldown_is_set_for_test("localhost"),
            "the cooldown must be recorded against localhost, the host that \
             actually sent the 429"
        );
        assert!(
            !fetcher.limiter.cooldown_is_set_for_test("127.0.0.1"),
            "the cooldown must not be recorded against 127.0.0.1, the \
             redirector — it never sent a 429 itself"
        );
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
        // NOT be laundered into the stable `Blocked` outcome. Also not
        // retried: a redirect-budget error is not `is_connect()`/`is_timeout()`
        // shaped, so `is_transient` classifies it fatal and the loop below
        // runs exactly once regardless of `max_retries`.
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

    // -----------------------------------------------------------------------
    // `HostLimiter::acquire` — the async waiting path (issue #207)
    //
    // `HostLimiter`'s own test module (`http::limiter`) cannot exercise this:
    // its `FakeRelativeClock` seam does not implement `ReasonablyRealtime`,
    // the bound `acquire` requires, so every timing assertion there is
    // synchronous (`check_ready`) and never actually awaits `acquire`. This
    // closes that gap using only public API, on the real clock, with a rate
    // high enough (100 req/s) to keep the wait small and the test fast.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn host_limiter_acquire_waits_for_a_public_host_but_not_for_loopback() {
        let settings = http::HttpSettings {
            requests_per_second: 100,
            burst: 1,
            ..http::HttpSettings::default()
        };
        let limiter = http::HostLimiter::new(&settings);

        let loopback = reqwest::Url::parse("http://127.0.0.1/doc").expect("must parse");
        let start = std::time::Instant::now();
        limiter.acquire(&loopback).await;
        limiter.acquire(&loopback).await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "loopback must never be paced, regardless of the configured rate"
        );

        // A globally-routable IP literal: `acquire` only waits on the token
        // bucket, it never connects, so this needs no network access despite
        // the address being real.
        let public = reqwest::Url::parse("http://1.1.1.1/doc").expect("must parse");
        limiter.acquire(&public).await; // consumes the sole burst token
        let start = std::time::Instant::now();
        limiter.acquire(&public).await; // must wait ~10ms at 100 req/s
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(5),
            "expected a measurable wait for a paced public host, got {waited:?}"
        );
        assert!(
            waited < Duration::from_millis(500),
            "the wait must stay small and bounded, got {waited:?}"
        );
    }
}
