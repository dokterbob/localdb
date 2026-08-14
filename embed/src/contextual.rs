//! The shared implementation behind the document-context ("contextualized")
//! hosted embedding providers — Perplexity and Voyage.
//!
//! Both providers speak the same protocol: POST a JSON body carrying a model
//! id, one document's full context, and that document's chunk list; receive
//! back a `data` array of `{embedding, index}` objects, one per chunk, in
//! unspecified order. They differ only in five constants (base URL, default
//! model, default dimension, endpoint path, provider name) and in **one JSON
//! field name** — Perplexity sends the chunk list as `chunks`, Voyage as
//! `input`.
//!
//! Those differences live in a [`ContextualProviderSpec`] impl per provider;
//! everything else — client construction, default resolution, auth headers,
//! serialization, retry delegation, response parsing, index reordering, and
//! the [`Embedder`] impl itself — is written once here as
//! [`ContextualEmbedder<P>`]. Each provider module exposes a type alias
//! (`pub type PerplexityEmbedder = ContextualEmbedder<Perplexity>`), so
//! callers are unaware of the generic.
//!
//! See specs/04-search-pipeline.md §4, specs/03-config.md §6.

use std::marker::PhantomData;

use async_trait::async_trait;
use fetch::http::HttpSettings;
use localdb_core::{DocumentChunks, EmbeddedDocument, Embedder, Error as CoreError};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::EmbedError;
use crate::http_helper::{build_hosted_client, send_with_retry};
use crate::retry::RetryPolicy;

/// Everything that distinguishes one contextualized provider from another.
///
/// Implemented by a zero-sized marker type per provider (`Perplexity`,
/// `Voyage`). The `Send + Sync + 'static` bound is what lets
/// `ContextualEmbedder<P>` satisfy `#[async_trait]`'s `dyn Future + Send`
/// requirement on the [`Embedder`] impl.
pub trait ContextualProviderSpec: Send + Sync + 'static {
    /// Production API origin, used unless overridden via
    /// [`ContextualEmbedder::with_base_url`].
    const DEFAULT_BASE_URL: &'static str;
    /// Model id used when the operator does not configure one.
    const DEFAULT_MODEL: &'static str;
    /// Embedding dimension of [`Self::DEFAULT_MODEL`].
    const DEFAULT_DIM: usize;
    /// Path appended to the base URL, leading slash included.
    const PATH: &'static str;
    /// Provider name as it appears in [`EmbedError`] messages.
    const NAME: &'static str;

    /// Build the request body.
    ///
    /// This is the one place these providers' requests genuinely diverge: the
    /// JSON key the chunk list is sent under (`chunks` vs. `input`). The
    /// `model` and `document` fields are identical, but each provider keeps
    /// its own `#[derive(Serialize)]` struct so its module doc block stays a
    /// faithful, checkable description of that provider's wire format.
    fn request<'a>(
        model: &'a str,
        document: &'a str,
        chunks: &'a [String],
    ) -> impl Serialize + Send + 'a;
}

/// One embedding object in a contextualized provider's response.
///
/// `index` is load-bearing: both APIs may return `data` out of order, so
/// embeddings are reordered into chunk order before being handed back.
#[derive(Debug, Deserialize)]
struct EmbeddingObject {
    embedding: Vec<f32>,
    index: usize,
}

/// Response envelope shared by both contextualized providers.
#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbeddingObject>,
}

/// A document-context hosted embedder, specialized by `P`.
///
/// Embeds one document per request: the document context (full text or
/// summary) travels alongside the chunk list, giving the model the broader
/// context that late/contextualized chunking needs.
pub struct ContextualEmbedder<P: ContextualProviderSpec> {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    embedding_dim: usize,
    http_settings: HttpSettings,
    _spec: PhantomData<P>,
}

impl<P: ContextualProviderSpec> ContextualEmbedder<P> {
    /// Create a new embedder, applying `P`'s model/dimension defaults.
    ///
    /// `retry` covers batching/timeout policy — only `request_timeout` is
    /// relevant here (these providers embed one document's chunks per
    /// request, so `batch_size` does not apply); it is consumed at
    /// construction and not retained. `http_settings` is the shared
    /// outgoing-HTTP policy (user agent, retry/backoff/jitter — see
    /// `fetch::http`) and is retained for use on every request.
    pub fn new(
        api_key: impl Into<String>,
        model: Option<String>,
        embedding_dim: Option<usize>,
        retry: RetryPolicy,
        http_settings: HttpSettings,
    ) -> Result<Self, EmbedError> {
        let client = build_hosted_client(&http_settings, retry.request_timeout)?;
        Ok(Self {
            client,
            base_url: P::DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            model: model.unwrap_or_else(|| P::DEFAULT_MODEL.to_string()),
            embedding_dim: embedding_dim.unwrap_or(P::DEFAULT_DIM),
            http_settings,
            _spec: PhantomData,
        })
    }

    /// Create from the environment variable holding the API key.
    ///
    /// Returns `None` if the environment variable is not set.
    pub fn from_env(api_key_env: &str) -> Option<Result<Self, EmbedError>> {
        std::env::var(api_key_env).ok().map(|key| {
            Self::new(
                key,
                None,
                None,
                RetryPolicy::default(),
                HttpSettings::default(),
            )
        })
    }

    /// Override the base URL (useful for testing).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Embed chunks for one document with document context.
    async fn embed_document_chunks(
        &self,
        document_context: &str,
        chunks: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if chunks.is_empty() {
            return Ok(vec![]);
        }

        let url = format!("{}{}", self.base_url.trim_end_matches('/'), P::PATH);

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|e| {
                EmbedError::Internal(format!("invalid authorization header value: {e}"))
            })?,
        );

        let body = serde_json::to_vec(&P::request(&self.model, document_context, chunks))
            .map_err(|e| self.provider_error(format!("failed to serialize request: {e}")))?;

        let response =
            send_with_retry(&self.client, &url, headers, body, &self.http_settings).await?;

        let resp: EmbedResponse = serde_json::from_slice(&response)
            .map_err(|e| self.provider_error(format!("failed to parse response: {e}")))?;

        let mut vecs: Vec<Option<Vec<f32>>> = vec![None; chunks.len()];
        for obj in resp.data {
            if obj.index < vecs.len() {
                vecs[obj.index] = Some(obj.embedding);
            }
        }
        let result: Option<Vec<Vec<f32>>> = vecs.into_iter().collect();
        result.ok_or_else(|| self.provider_error("response missing some embedding indices"))
    }

    fn provider_error(&self, message: impl Into<String>) -> EmbedError {
        EmbedError::ProviderError {
            provider: P::NAME.to_string(),
            message: message.into(),
        }
    }
}

#[async_trait]
impl<P: ContextualProviderSpec> Embedder for ContextualEmbedder<P> {
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, CoreError> {
        if docs.is_empty() {
            return Ok(vec![]);
        }

        let mut results = Vec::with_capacity(docs.len());
        for doc in &docs {
            let embeddings = self
                .embed_document_chunks(&doc.document_context, &doc.chunks)
                .await
                .map_err(CoreError::from)?;
            results.push(embeddings);
        }
        Ok(results)
    }

    fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    fn model_id(&self) -> &str {
        &self.model
    }
}

/// Stand up the whole contextualized-provider test suite for one provider:
/// `$spec` is its [`ContextualProviderSpec`] type, `$chunks_field` the JSON
/// key it sends its chunk list under.
///
/// Every case lives in [`test_support`] and is generic over the spec, so this
/// expands to one call apiece. It is a macro rather than a second set of
/// hand-written generic wrappers because those wrappers would be identical
/// between `perplexity` and `voyage` down to the type parameter — reinstating,
/// in the test modules, precisely the clone this module exists to remove.
///
/// Each case stands up its mock at `$spec`'s `PATH`, so the suite also pins
/// every provider's endpoint. The one assertion that cannot be provider-
/// agnostic is the chunk field itself (Perplexity sends `chunks`, Voyage
/// `input`); it is a macro argument so the difference stays visible, and
/// asserted, at each call site.
#[cfg(test)]
macro_rules! contextual_provider_tests {
    ($spec:ty, $chunks_field:literal) => {
        #[cfg(test)]
        mod tests {
            use super::*;
            use $crate::contextual::test_support as shared;

            #[tokio::test]
            async fn correct_shape() {
                shared::correct_shape::<$spec>().await;
            }

            #[tokio::test]
            async fn passes_document_context() {
                shared::passes_document_context::<$spec>().await;
            }

            #[tokio::test]
            async fn retries_on_429() {
                shared::retries_on_429::<$spec>().await;
            }

            #[tokio::test]
            async fn fails_after_max_retries() {
                shared::fails_after_max_retries::<$spec>().await;
            }

            #[tokio::test]
            async fn multiple_docs_sequential() {
                shared::multiple_docs_sequential::<$spec>().await;
            }

            #[tokio::test]
            async fn timeout_returns_provider_unavailable() {
                shared::timeout_returns_provider_unavailable::<$spec>().await;
            }

            #[tokio::test]
            async fn empty_docs() {
                shared::empty_docs::<$spec>().await;
            }

            #[test]
            fn reports_its_own_defaults() {
                shared::defaults_are_reported::<$spec>();
            }

            #[tokio::test]
            async fn sends_chunk_list_under_its_own_field() {
                shared::sends_chunks_under::<$spec>($chunks_field).await;
            }
        }
    };
}

#[cfg(test)]
pub(crate) use contextual_provider_tests;

/// Provider-agnostic test bodies, instantiated once per provider by
/// [`contextual_provider_tests!`].
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use localdb_core::DocumentChunks;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// `HttpSettings` for tests that force a retry: `min_retry_delay` is
    /// dialed down to millisecond scale so the jittered exponential backoff
    /// `fetch::http::retry_policy` builds never adds more than a few
    /// milliseconds of real sleep — see `HttpSettings::min_retry_delay`'s
    /// doc comment, this is exactly the test seam it exists for.
    fn fast_http_settings(max_retries: u32) -> HttpSettings {
        HttpSettings {
            max_retries,
            min_retry_delay: std::time::Duration::from_millis(1),
            ..HttpSettings::default()
        }
    }

    fn retry_policy(request_timeout: std::time::Duration) -> RetryPolicy {
        RetryPolicy {
            request_timeout,
            batch_size: 32,
        }
    }

    /// A well-formed response body carrying `n` embeddings of `dim` floats.
    pub(crate) fn make_response(n: usize, dim: usize) -> serde_json::Value {
        let data: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({ "embedding": vec![0.2f32; dim], "index": i }))
            .collect();
        serde_json::json!({ "data": data })
    }

    /// An embedder pointed at `server_uri`, generous on timeout and retries.
    pub(crate) fn make_embedder<P: ContextualProviderSpec>(
        server_uri: &str,
    ) -> ContextualEmbedder<P> {
        ContextualEmbedder::<P>::new(
            "test-api-key",
            None,
            Some(P::DEFAULT_DIM),
            retry_policy(std::time::Duration::from_secs(5)),
            fast_http_settings(3),
        )
        .expect("failed to construct embedder")
        .with_base_url(server_uri)
    }

    fn one_doc(context: &str, chunks: &[&str]) -> Vec<DocumentChunks> {
        vec![DocumentChunks {
            document_context: context.to_string(),
            chunks: chunks.iter().map(|c| c.to_string()).collect(),
        }]
    }

    /// A happy-path response yields one vector per chunk, at `P`'s dimension.
    pub(crate) async fn correct_shape<P: ContextualProviderSpec>() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(2, P::DEFAULT_DIM)),
            )
            .mount(&server)
            .await;

        let embedder = make_embedder::<P>(&server.uri());
        let result = embedder
            .embed_documents(one_doc(
                "Full document about Rust",
                &["chunk one", "chunk two"],
            ))
            .await
            .expect("happy path should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 2);
        assert_eq!(result[0][0].len(), P::DEFAULT_DIM);
    }

    /// The document context is sent in the `document` field — the whole point
    /// of a contextualized provider. The mock only matches if it is present,
    /// so a missing field surfaces as a failed request, not a silent pass.
    pub(crate) async fn passes_document_context<P: ContextualProviderSpec>() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "document": "important document context"
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(1, P::DEFAULT_DIM)),
            )
            .mount(&server)
            .await;

        let embedder = make_embedder::<P>(&server.uri());
        let result = embedder
            .embed_documents(one_doc("important document context", &["relevant chunk"]))
            .await;
        assert!(
            result.is_ok(),
            "contextualized request should succeed: {result:?}"
        );
    }

    /// A single 429 is retried rather than surfaced (issue #207).
    pub(crate) async fn retries_on_429<P: ContextualProviderSpec>() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(1, P::DEFAULT_DIM)),
            )
            .mount(&server)
            .await;

        let embedder = make_embedder::<P>(&server.uri());
        let result = embedder.embed_documents(one_doc("ctx", &["text"])).await;
        assert!(result.is_ok(), "should succeed after retry: {result:?}");
    }

    /// A persistently-503 endpoint exhausts retries and surfaces as
    /// `provider_unavailable` — the provider is down, as opposed to rate
    /// limiting us, which `http_helper` classifies separately.
    pub(crate) async fn fails_after_max_retries<P: ContextualProviderSpec>() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let embedder = ContextualEmbedder::<P>::new(
            "test-key",
            None,
            Some(P::DEFAULT_DIM),
            retry_policy(std::time::Duration::from_secs(5)),
            fast_http_settings(1),
        )
        .expect("failed to construct embedder")
        .with_base_url(server.uri());

        let result = embedder.embed_documents(one_doc("ctx", &["text"])).await;
        assert_eq!(
            result.expect_err("persistent 503 should fail").code(),
            "provider_unavailable"
        );
    }

    /// Multiple documents are embedded one request each, in order.
    pub(crate) async fn multiple_docs_sequential<P: ContextualProviderSpec>() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(2, P::DEFAULT_DIM)),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(1, P::DEFAULT_DIM)),
            )
            .mount(&server)
            .await;

        let embedder = make_embedder::<P>(&server.uri());
        let docs = vec![
            DocumentChunks {
                document_context: "doc1 context".to_string(),
                chunks: vec!["a".to_string(), "b".to_string()],
            },
            DocumentChunks {
                document_context: "doc2 context".to_string(),
                chunks: vec!["c".to_string()],
            },
        ];

        let result = embedder
            .embed_documents(docs)
            .await
            .expect("both should embed");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 2);
        assert_eq!(result[1].len(), 1);
    }

    /// A request that outlives `request_timeout` is *classified* as
    /// `provider_unavailable`. 0 retries, so this pins classification rather
    /// than retry behavior.
    pub(crate) async fn timeout_returns_provider_unavailable<P: ContextualProviderSpec>() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(make_response(1, P::DEFAULT_DIM))
                    .set_delay(std::time::Duration::from_secs(5)),
            )
            .mount(&server)
            .await;

        let embedder = ContextualEmbedder::<P>::new(
            "test-key",
            None,
            Some(P::DEFAULT_DIM),
            retry_policy(std::time::Duration::from_millis(50)),
            fast_http_settings(0),
        )
        .expect("failed to construct embedder")
        .with_base_url(server.uri());

        let result = embedder.embed_documents(one_doc("ctx", &["text"])).await;
        assert_eq!(
            result.expect_err("timed-out request should fail").code(),
            "provider_unavailable",
            "timeout should surface as provider_unavailable"
        );
    }

    /// No documents means no requests and no error.
    pub(crate) async fn empty_docs<P: ContextualProviderSpec>() {
        let server = MockServer::start().await;
        let embedder = make_embedder::<P>(&server.uri());
        let result = embedder
            .embed_documents(vec![])
            .await
            .expect("empty input should succeed");
        assert!(result.is_empty());
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty(),
            "no documents means no HTTP requests"
        );
    }

    /// With nothing configured, `P`'s defaults are what the embedder reports.
    pub(crate) fn defaults_are_reported<P: ContextualProviderSpec>() {
        let embedder = ContextualEmbedder::<P>::new(
            "key",
            None,
            None,
            RetryPolicy::default(),
            HttpSettings::default(),
        )
        .expect("failed to construct embedder");
        assert_eq!(embedder.model_id(), P::DEFAULT_MODEL);
        assert_eq!(embedder.embedding_dim(), P::DEFAULT_DIM);
    }

    /// Assert the chunk list is sent under `field` — the single wire-level
    /// difference between the two providers, so each calls this with its own
    /// key and a body matcher that fails the request if the key is missing.
    pub(crate) async fn sends_chunks_under<P: ContextualProviderSpec>(field: &str) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(P::PATH))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({ field: ["only chunk"] }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(make_response(1, P::DEFAULT_DIM)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let embedder = make_embedder::<P>(&server.uri());
        let result = embedder
            .embed_documents(one_doc("ctx", &["only chunk"]))
            .await;
        assert!(
            result.is_ok(),
            "{} must send its chunk list under {field:?}: {result:?}",
            P::NAME
        );
    }
}
