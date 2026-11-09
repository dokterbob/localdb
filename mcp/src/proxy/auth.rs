//! A pluggable bearer source for the upstream Streamable HTTP transport, and
//! an `rmcp::StreamableHttpClient` that retries a request exactly once when
//! the upstream rejects the token it sent.
//!
//! `ProxyHandler::connect_with_auth` holds the upstream `RunningService` for
//! as long as the stdio process runs — potentially far longer than any one
//! access token's lifetime. A transport built from a single static bearer
//! (`StreamableHttpClientTransportConfig::auth_header`) has no way to pick
//! up a token refreshed by another process (e.g. a concurrent `localdb`
//! invocation redeeming the same refresh token), so every call made after
//! the token expires fails with "Auth required" for the rest of the
//! session. `RefreshingHttpClient` asks its `BearerSource` for a token on
//! every call instead of baking one in, and reacts to a 401 by asking the
//! source to refresh and retrying once.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Error as SseError, Sse};

/// Where `RefreshingHttpClient` gets the bearer token to send, and how it
/// gets a new one once the upstream rejects the current one.
#[async_trait]
pub trait BearerSource: Send + Sync + 'static {
    /// The token to send with the next request. Implementations that cache
    /// a token proactively refresh it here once it is known to be expired,
    /// rather than waiting for the upstream to reject it.
    async fn current(&self) -> Option<String>;

    /// Force a refresh because `rejected` (the token a request just sent)
    /// was turned down by the upstream. Returns the replacement, or `None`
    /// if no replacement is available — in which case the caller must not
    /// retry.
    async fn refresh(&self, rejected: &str) -> Option<String>;
}

/// A fixed bearer token, never refreshed — for a caller that holds its own
/// secret outside the credentials-file refresh flow (a static
/// `LOCALDB_API_KEY`, a test harness minting its own key).
pub struct StaticBearer(pub String);

#[async_trait]
impl BearerSource for StaticBearer {
    async fn current(&self) -> Option<String> {
        Some(self.0.clone())
    }

    async fn refresh(&self, _rejected: &str) -> Option<String> {
        None
    }
}

/// An `rmcp::StreamableHttpClient` that asks a [`BearerSource`] for a token
/// on every call and retries a rejected call exactly once with a refreshed
/// token.
///
/// Every method ignores the `auth_header` the transport worker passes in —
/// the whole point is that the token comes from `source`, which can change
/// between calls, not from a value fixed at transport-construction time.
#[derive(Clone)]
pub(crate) struct RefreshingHttpClient {
    inner: reqwest::Client,
    source: Arc<dyn BearerSource>,
}

impl RefreshingHttpClient {
    pub(crate) fn new(source: Arc<dyn BearerSource>) -> Self {
        Self {
            // Mirrors rmcp's own default Streamable HTTP client
            // (`StreamableHttpClientTransport::from_config`): disables idle
            // connection pooling, which avoids a ~40ms stall from TCP
            // Delayed ACK on Linux when a previous response body wasn't
            // fully drained before the pool tried to reuse the connection.
            inner: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .expect("building the proxy's reqwest client"),
            source,
        }
    }
}

impl StreamableHttpClient for RefreshingHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let sent = self.source.current().await;
        let first = <reqwest::Client as StreamableHttpClient>::post_message(
            &self.inner,
            uri.clone(),
            message.clone(),
            session_id.clone(),
            sent.clone(),
            custom_headers.clone(),
        )
        .await;
        match first {
            Err(err) if is_auth_rejection(&err) => match self.retry_token(sent).await {
                Some(new) => {
                    <reqwest::Client as StreamableHttpClient>::post_message(
                        &self.inner,
                        uri,
                        message,
                        session_id,
                        Some(new),
                        custom_headers,
                    )
                    .await
                }
                None => Err(err),
            },
            other => other,
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        // No retry: a rejected delete leaves the session as it was, and a
        // second attempt with a refreshed token isn't worth the extra
        // round trip for a call whose result nothing here inspects.
        let tok = self.source.current().await;
        <reqwest::Client as StreamableHttpClient>::delete_session(
            &self.inner,
            uri,
            session_id,
            tok,
            custom_headers,
        )
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let sent = self.source.current().await;
        let first = <reqwest::Client as StreamableHttpClient>::get_stream(
            &self.inner,
            uri.clone(),
            session_id.clone(),
            last_event_id.clone(),
            sent.clone(),
            custom_headers.clone(),
        )
        .await;
        match first {
            Err(err) if is_auth_rejection(&err) => match self.retry_token(sent).await {
                Some(new) => {
                    <reqwest::Client as StreamableHttpClient>::get_stream(
                        &self.inner,
                        uri,
                        session_id,
                        last_event_id,
                        Some(new),
                        custom_headers,
                    )
                    .await
                }
                None => Err(err),
            },
            other => other,
        }
    }
}

/// True when a request failed because the upstream rejected its bearer
/// token, in either of the two shapes rmcp's reqwest client produces for a
/// 401: `AuthRequired` (from `post_message`, which recognizes a 401 that
/// carries `WWW-Authenticate`) or a bare `Client` error (from `get_stream`,
/// whose `response.error_for_status()` call turns any 401 into one,
/// `WWW-Authenticate` or not). Either way, asking the source for a fresh
/// token and retrying once is the right response.
fn is_auth_rejection(error: &StreamableHttpError<reqwest::Error>) -> bool {
    match error {
        StreamableHttpError::AuthRequired(_) => true,
        StreamableHttpError::Client(e) => e.status() == Some(reqwest::StatusCode::UNAUTHORIZED),
        _ => false,
    }
}

impl RefreshingHttpClient {
    /// The replacement token for a single retry, or `None` when the caller
    /// must give up: no token was sent in the first place, the source has
    /// nothing new to offer, or it handed back the exact token that was
    /// just rejected (retrying with the same token would just fail again).
    async fn retry_token(&self, sent: Option<String>) -> Option<String> {
        let old = sent?;
        let new = self.source.refresh(&old).await?;
        (new != old).then_some(new)
    }
}

#[cfg(test)]
mod tests;
