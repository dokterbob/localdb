//! Perplexity contextualized embedding provider.
//!
//! Uses Perplexity's `/v1/contextualizedembeddings` endpoint, which accepts a document
//! context and a list of chunks, returning contextualized embeddings for each chunk.
//!
//! The API key is read from the environment variable specified in config (`api_key_env`).
//!
//! # API shape (as documented by Perplexity)
//!
//! Request:
//! ```json
//! {
//!   "model": "pplx-embed-context-v1",
//!   "document": "full document text",
//!   "chunks": ["chunk 1", "chunk 2", ...]
//! }
//! ```
//!
//! Response:
//! ```json
//! {
//!   "data": [
//!     {"embedding": [0.1, ...], "index": 0},
//!     ...
//!   ]
//! }
//! ```
//!
//! See specs/04-search-pipeline.md §4, specs/03-config.md §6.

use async_trait::async_trait;
use localdb_core::{DocumentChunks, EmbeddedDocument, Embedder, Error as CoreError};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::EmbedError;
use crate::retry::RetryPolicy;

const DEFAULT_BASE_URL: &str = "https://api.perplexity.ai";
const DEFAULT_MODEL: &str = "pplx-embed-context-v1";
const DEFAULT_DIM: usize = 768;

/// Request body for Perplexity `/v1/contextualizedembeddings`.
#[derive(Debug, Serialize)]
struct PerplexityEmbedRequest<'a> {
    model: &'a str,
    document: &'a str,
    chunks: &'a [String],
}

/// One embedding object in the response.
#[derive(Debug, Deserialize)]
struct PerplexityEmbeddingObject {
    embedding: Vec<f32>,
    index: usize,
}

/// Response from Perplexity `/v1/contextualizedembeddings`.
#[derive(Debug, Deserialize)]
struct PerplexityEmbedResponse {
    data: Vec<PerplexityEmbeddingObject>,
}

/// Perplexity contextualized embedding provider.
///
/// Uses document context for each document's chunks. The document context
/// (full text or summary) is passed alongside the chunk list, giving the model
/// the broader context needed for late/contextualized chunking.
pub struct PerplexityEmbedder {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    embedding_dim: usize,
    retry: RetryPolicy,
}

impl PerplexityEmbedder {
    /// Create a new Perplexity embedder.
    pub fn new(
        api_key: impl Into<String>,
        model: Option<String>,
        embedding_dim: Option<usize>,
        retry: RetryPolicy,
    ) -> Self {
        let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let embedding_dim = embedding_dim.unwrap_or(DEFAULT_DIM);
        let client = Client::builder()
            .timeout(retry.request_timeout)
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            model,
            embedding_dim,
            retry,
        }
    }

    /// Create from environment variable for the API key.
    ///
    /// Returns `None` if the environment variable is not set.
    pub fn from_env(api_key_env: &str) -> Option<Self> {
        std::env::var(api_key_env)
            .ok()
            .map(|key| Self::new(key, None, None, RetryPolicy::default()))
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

        let url = format!(
            "{}/v1/contextualizedembeddings",
            self.base_url.trim_end_matches('/')
        );

        let mut last_error = String::new();
        let mut attempt = 0u32;

        loop {
            if attempt > 0 {
                let backoff = self.retry.backoff_for_attempt(attempt - 1);
                debug!(
                    attempt,
                    backoff_ms = backoff.as_millis(),
                    "retrying perplexity embedding request"
                );
                tokio::time::sleep(backoff).await;
            }

            let body = PerplexityEmbedRequest {
                model: &self.model,
                document: document_context,
                chunks,
            };

            match self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if response.status().is_success() {
                        let resp: PerplexityEmbedResponse =
                            response
                                .json()
                                .await
                                .map_err(|e| EmbedError::ProviderError {
                                    provider: "perplexity".to_string(),
                                    message: format!("failed to parse response: {e}"),
                                })?;

                        let mut vecs: Vec<Option<Vec<f32>>> = vec![None; chunks.len()];
                        for obj in resp.data {
                            if obj.index < vecs.len() {
                                vecs[obj.index] = Some(obj.embedding);
                            }
                        }
                        let result: Option<Vec<Vec<f32>>> = vecs.into_iter().collect();
                        return result.ok_or_else(|| EmbedError::ProviderError {
                            provider: "perplexity".to_string(),
                            message: "response missing some embedding indices".to_string(),
                        });
                    } else if self.retry.should_retry_status(status)
                        && attempt + 1 < self.retry.max_attempts
                    {
                        let body_text = response.text().await.unwrap_or_default();
                        warn!(status, "perplexity request failed, will retry");
                        last_error = format!("HTTP {status}: {body_text}");
                        attempt += 1;
                        continue;
                    } else {
                        let body_text = response.text().await.unwrap_or_default();
                        if attempt + 1 >= self.retry.max_attempts && !last_error.is_empty() {
                            return Err(EmbedError::RetriesExhausted {
                                provider: "perplexity".to_string(),
                                attempts: attempt + 1,
                                last_error,
                            });
                        }
                        return Err(EmbedError::ProviderError {
                            provider: "perplexity".to_string(),
                            message: format!("HTTP {status}: {body_text}"),
                        });
                    }
                }
                Err(e) if e.is_timeout() => {
                    warn!("perplexity request timed out");
                    if attempt + 1 >= self.retry.max_attempts {
                        return Err(EmbedError::Timeout {
                            provider: "perplexity".to_string(),
                            timeout_secs: self.retry.request_timeout.as_secs(),
                        });
                    }
                    last_error = e.to_string();
                    attempt += 1;
                }
                Err(e) => {
                    warn!(%e, "perplexity request failed");
                    if attempt + 1 >= self.retry.max_attempts {
                        return Err(EmbedError::RetriesExhausted {
                            provider: "perplexity".to_string(),
                            attempts: attempt + 1,
                            last_error: e.to_string(),
                        });
                    }
                    last_error = e.to_string();
                    attempt += 1;
                }
            }

            if attempt >= self.retry.max_attempts {
                return Err(EmbedError::RetriesExhausted {
                    provider: "perplexity".to_string(),
                    attempts: attempt,
                    last_error,
                });
            }
        }
    }
}

#[async_trait]
impl Embedder for PerplexityEmbedder {
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

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::{DocumentChunks, Embedder};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_response(n: usize, dim: usize) -> serde_json::Value {
        let data: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "embedding": vec![0.2f32; dim],
                    "index": i
                })
            })
            .collect();
        serde_json::json!({ "data": data })
    }

    fn make_embedder(server_uri: &str) -> PerplexityEmbedder {
        PerplexityEmbedder::new(
            "test-api-key",
            None,
            Some(768),
            RetryPolicy {
                max_attempts: 3,
                initial_backoff: std::time::Duration::from_millis(10),
                request_timeout: std::time::Duration::from_secs(5),
                batch_size: 32,
            },
        )
        .with_base_url(server_uri)
    }

    #[tokio::test]
    async fn perplexity_embedder_correct_shape() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/contextualizedembeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(2, 768)))
            .mount(&server)
            .await;

        let embedder = make_embedder(&server.uri());
        let docs = vec![DocumentChunks {
            document_context: "Full document about Rust".to_string(),
            chunks: vec!["chunk one".to_string(), "chunk two".to_string()],
        }];

        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 2);
        assert_eq!(result[0][0].len(), 768);
    }

    #[tokio::test]
    async fn perplexity_embedder_passes_document_context() {
        let server = MockServer::start().await;

        // Verify the request includes document field by using body_partial_json matcher
        Mock::given(method("POST"))
            .and(path("/v1/contextualizedembeddings"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "document": "important document context"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 768)))
            .mount(&server)
            .await;

        let embedder = make_embedder(&server.uri());
        let docs = vec![DocumentChunks {
            document_context: "important document context".to_string(),
            chunks: vec!["relevant chunk".to_string()],
        }];

        let result = embedder.embed_documents(docs).await;
        assert!(
            result.is_ok(),
            "contextualized request should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn perplexity_embedder_retries_on_429() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/contextualizedembeddings"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/contextualizedembeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 768)))
            .mount(&server)
            .await;

        let embedder = make_embedder(&server.uri());
        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["text".to_string()],
        }];
        let result = embedder.embed_documents(docs).await;
        assert!(result.is_ok(), "should succeed after retry: {result:?}");
    }

    #[tokio::test]
    async fn perplexity_embedder_fails_after_max_retries() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/contextualizedembeddings"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let embedder = PerplexityEmbedder::new(
            "test-key",
            None,
            Some(768),
            RetryPolicy {
                max_attempts: 2,
                initial_backoff: std::time::Duration::from_millis(10),
                request_timeout: std::time::Duration::from_secs(5),
                batch_size: 32,
            },
        )
        .with_base_url(server.uri());

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["text".to_string()],
        }];
        let result = embedder.embed_documents(docs).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "provider_unavailable");
    }

    #[tokio::test]
    async fn perplexity_embedder_multiple_docs_sequential() {
        let server = MockServer::start().await;

        // Two separate calls (one per doc)
        Mock::given(method("POST"))
            .and(path("/v1/contextualizedembeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(2, 768)))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/contextualizedembeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 768)))
            .mount(&server)
            .await;

        let embedder = make_embedder(&server.uri());
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

        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 2);
        assert_eq!(result[1].len(), 1);
    }

    #[tokio::test]
    async fn perplexity_embedder_empty_docs() {
        let server = MockServer::start().await;
        let embedder = make_embedder(&server.uri());
        let result = embedder.embed_documents(vec![]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn perplexity_embedder_model_id() {
        let embedder = PerplexityEmbedder::new("key", None, None, RetryPolicy::default());
        assert_eq!(embedder.model_id(), DEFAULT_MODEL);
        assert_eq!(embedder.embedding_dim(), DEFAULT_DIM);
    }
}
