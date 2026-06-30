//! OpenAI-compatible flat embedding provider.
//!
//! Targets any `/v1/embeddings`-compatible endpoint: OpenAI, Azure OpenAI,
//! Ollama, LiteLLM, etc.
//!
//! This is the **flat (context-free) path**: document context is ignored,
//! each chunk is embedded independently. It is the degenerate case of the
//! document-aware `Embedder` trait.
//!
//! # Configuration
//!
//! - `base_url`: endpoint base URL (e.g. `https://api.openai.com`)
//! - `api_key`: bearer token (from `api_key_env` in config — never inlined)
//! - `model`: model name (e.g. `text-embedding-3-small`)
//! - `dim`: optional dimension (for models that support truncation)
//!
//! See specs/03-config.md §1, §6.

use async_trait::async_trait;
use localdb_core::{DocumentChunks, EmbeddedDocument, Embedder, Error as CoreError};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::EmbedError;
use crate::http_helper::send_with_retry;
use crate::retry::RetryPolicy;

/// Request body for `/v1/embeddings`.
#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    input: &'a [String],
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

/// One embedding object in the response.
#[derive(Debug, Deserialize)]
struct EmbeddingObject {
    embedding: Vec<f32>,
    index: usize,
}

/// Response from `/v1/embeddings`.
#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbeddingObject>,
}

/// OpenAI-compatible flat embedding provider.
///
/// Context-free: each chunk is embedded independently. The document context
/// from [`DocumentChunks`] is not used.
pub struct OpenAiEmbedder {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    dimensions: Option<usize>,
    embedding_dim: usize,
    retry: RetryPolicy,
}

impl OpenAiEmbedder {
    /// Create a new OpenAI-compatible embedder.
    ///
    /// # Arguments
    /// * `base_url` - Endpoint base URL, e.g. `https://api.openai.com`
    /// * `api_key` - Bearer token, or `None` for unauthenticated endpoints
    /// * `model` - Model name, e.g. `text-embedding-3-small`
    /// * `embedding_dim` - Expected embedding dimension
    /// * `dimensions` - Optional dimension for truncation
    /// * `retry` - Retry/timeout policy
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        embedding_dim: usize,
        dimensions: Option<usize>,
        retry: RetryPolicy,
    ) -> Result<Self, EmbedError> {
        let client = Client::builder()
            .timeout(retry.request_timeout)
            .build()
            .map_err(|e| EmbedError::Internal(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url: base_url.into(),
            api_key,
            model: model.into(),
            dimensions,
            embedding_dim,
            retry,
        })
    }

    /// Build from the standard `providers` config entry.
    ///
    /// The API key is read from the environment variable named by `api_key_env`.
    /// See specs/03-config.md §6.
    pub fn from_config(
        base_url: impl Into<String>,
        api_key_env: Option<&str>,
        model: impl Into<String>,
        embedding_dim: usize,
    ) -> Result<Self, EmbedError> {
        let api_key = api_key_env.and_then(|env| std::env::var(env).ok());
        Self::new(
            base_url,
            api_key,
            model,
            embedding_dim,
            None,
            RetryPolicy::default(),
        )
    }

    /// Embed a batch of texts (raw strings), returning vectors in the same order.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'));
        let request = EmbedRequest {
            input: texts,
            model: &self.model,
            dimensions: self.dimensions,
        };

        let body = serde_json::to_vec(&request).map_err(|e| {
            EmbedError::Internal(format!("failed to serialize embedding request: {e}"))
        })?;

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(key) = &self.api_key {
            let auth_value = format!("Bearer {key}");
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&auth_value).map_err(|e| {
                    EmbedError::Internal(format!("invalid authorization header: {e}"))
                })?,
            );
        }

        let response_bytes =
            send_with_retry(&self.client, &url, headers, body, &self.retry).await?;
        let resp: EmbedResponse =
            serde_json::from_slice(&response_bytes).map_err(|e| EmbedError::ProviderError {
                provider: "openai-compatible".to_string(),
                message: format!("failed to parse response: {e}"),
            })?;

        // Reorder by index (API doesn't guarantee order)
        let mut vecs: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for obj in resp.data {
            if obj.index < vecs.len() {
                vecs[obj.index] = Some(obj.embedding);
            }
        }
        let result: Option<Vec<Vec<f32>>> = vecs.into_iter().collect();
        result.ok_or_else(|| EmbedError::ProviderError {
            provider: "openai-compatible".to_string(),
            message: "response missing some embedding indices".to_string(),
        })
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    async fn embed_documents(
        &self,
        docs: Vec<DocumentChunks>,
    ) -> Result<Vec<EmbeddedDocument>, CoreError> {
        if docs.is_empty() {
            return Ok(vec![]);
        }

        // Flatten all chunks from all docs, recording offsets
        let mut all_chunks: Vec<String> = Vec::new();
        let mut doc_offsets: Vec<(usize, usize)> = Vec::new(); // (start, len) per doc

        for doc in &docs {
            let start = all_chunks.len();
            all_chunks.extend(doc.chunks.iter().cloned());
            doc_offsets.push((start, doc.chunks.len()));
        }

        if all_chunks.is_empty() {
            return Ok(docs.iter().map(|_| vec![]).collect());
        }

        // Embed in batches
        let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(all_chunks.len());
        for batch in all_chunks.chunks(self.retry.batch_size) {
            let vecs = self.embed_batch(batch).await.map_err(CoreError::from)?;
            all_embeddings.extend(vecs);
        }

        // Re-group by document
        let result = doc_offsets
            .into_iter()
            .map(|(start, len)| all_embeddings[start..start + len].to_vec())
            .collect();

        Ok(result)
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
                    "embedding": vec![0.1f32; dim],
                    "index": i,
                    "object": "embedding"
                })
            })
            .collect();
        serde_json::json!({
            "object": "list",
            "data": data,
            "model": "text-embedding-3-small",
            "usage": {"prompt_tokens": n * 10, "total_tokens": n * 10}
        })
    }

    #[tokio::test]
    async fn openai_embedder_returns_correct_shape() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(2, 1536)))
            .mount(&server)
            .await;

        let embedder = OpenAiEmbedder::new(
            server.uri(),
            None,
            "text-embedding-3-small",
            1536,
            None,
            RetryPolicy::default(),
        )
        .expect("failed to construct embedder");

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["chunk one".to_string(), "chunk two".to_string()],
        }];

        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 1, "one doc");
        assert_eq!(result[0].len(), 2, "two chunks");
        assert_eq!(result[0][0].len(), 1536, "dim 1536");
    }

    #[tokio::test]
    async fn openai_embedder_multi_doc() {
        let server = MockServer::start().await;

        // Server responds to two batch requests (batch_size=32, both docs fit in one)
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(3, 64)))
            .mount(&server)
            .await;

        let policy = RetryPolicy {
            batch_size: 32,
            ..Default::default()
        };
        let embedder = OpenAiEmbedder::new(server.uri(), None, "test-model", 64, None, policy)
            .expect("failed to construct embedder");

        let docs = vec![
            DocumentChunks {
                document_context: "doc1".to_string(),
                chunks: vec!["a".to_string(), "b".to_string()],
            },
            DocumentChunks {
                document_context: "doc2".to_string(),
                chunks: vec!["c".to_string()],
            },
        ];

        let result = embedder.embed_documents(docs).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 2);
        assert_eq!(result[1].len(), 1);
    }

    #[tokio::test]
    async fn openai_embedder_retries_on_429() {
        let server = MockServer::start().await;

        // First request returns 429, second succeeds
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 64)))
            .mount(&server)
            .await;

        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: std::time::Duration::from_millis(10),
            request_timeout: std::time::Duration::from_secs(5),
            batch_size: 32,
        };
        let embedder = OpenAiEmbedder::new(server.uri(), None, "test-model", 64, None, policy)
            .expect("failed to construct embedder");

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["text".to_string()],
        }];
        let result = embedder.embed_documents(docs).await;
        assert!(result.is_ok(), "should succeed after retry: {result:?}");
    }

    #[tokio::test]
    async fn openai_embedder_fails_after_max_retries() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
            .mount(&server)
            .await;

        let policy = RetryPolicy {
            max_attempts: 2,
            initial_backoff: std::time::Duration::from_millis(10),
            request_timeout: std::time::Duration::from_secs(5),
            batch_size: 32,
        };
        let embedder = OpenAiEmbedder::new(server.uri(), None, "test-model", 64, None, policy)
            .expect("failed to construct embedder");

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["text".to_string()],
        }];
        let result = embedder.embed_documents(docs).await;
        assert!(result.is_err(), "should fail after max retries");
        let core_err = result.unwrap_err();
        assert_eq!(core_err.code(), "provider_unavailable");
    }

    #[tokio::test]
    async fn openai_embedder_401_not_retried() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: std::time::Duration::from_millis(10),
            request_timeout: std::time::Duration::from_secs(5),
            batch_size: 32,
        };
        let embedder = OpenAiEmbedder::new(server.uri(), None, "test-model", 64, None, policy)
            .expect("failed to construct embedder");

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["text".to_string()],
        }];
        let result = embedder.embed_documents(docs).await;
        assert!(result.is_err(), "401 should fail");
        // Should report provider_unavailable
        assert_eq!(result.unwrap_err().code(), "provider_unavailable");
    }

    #[tokio::test]
    async fn openai_embedder_empty_docs() {
        let server = MockServer::start().await;
        // No mock needed — should return early
        let embedder = OpenAiEmbedder::new(
            server.uri(),
            None,
            "test-model",
            64,
            None,
            RetryPolicy::default(),
        )
        .expect("failed to construct embedder");
        let result = embedder.embed_documents(vec![]).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn openai_embedder_model_id() {
        let embedder = OpenAiEmbedder::new(
            "https://api.openai.com",
            None,
            "text-embedding-3-large",
            3072,
            None,
            RetryPolicy::default(),
        )
        .expect("failed to construct embedder");
        assert_eq!(embedder.model_id(), "text-embedding-3-large");
        assert_eq!(embedder.embedding_dim(), 3072);
    }

    #[tokio::test]
    async fn openai_embedder_sends_bearer_auth() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer test-key-123",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 64)))
            .mount(&server)
            .await;

        let embedder = OpenAiEmbedder::new(
            server.uri(),
            Some("test-key-123".to_string()),
            "test-model",
            64,
            None,
            RetryPolicy::default(),
        )
        .expect("failed to construct embedder");

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["text".to_string()],
        }];
        let result = embedder.embed_documents(docs).await;
        assert!(
            result.is_ok(),
            "request with auth key should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn openai_embedder_timeout_returns_provider_unavailable() {
        let server = MockServer::start().await;

        // Respond with a delay longer than the client timeout
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(make_response(1, 64))
                    .set_delay(std::time::Duration::from_secs(5)),
            )
            .mount(&server)
            .await;

        let policy = RetryPolicy {
            max_attempts: 1,
            initial_backoff: std::time::Duration::from_millis(10),
            // Very short timeout so the delayed response triggers a timeout
            request_timeout: std::time::Duration::from_millis(50),
            batch_size: 32,
        };
        let embedder = OpenAiEmbedder::new(server.uri(), None, "test-model", 64, None, policy)
            .expect("failed to construct embedder");

        let docs = vec![DocumentChunks {
            document_context: "ctx".to_string(),
            chunks: vec!["text".to_string()],
        }];
        let result = embedder.embed_documents(docs).await;
        assert!(result.is_err(), "timed-out request should fail");
        assert_eq!(
            result.unwrap_err().code(),
            "provider_unavailable",
            "timeout should surface as provider_unavailable"
        );
    }

    #[tokio::test]
    async fn openai_embedder_batches_chunks() {
        let server = MockServer::start().await;

        // With batch_size=2 and 3 total chunks, should send 2 requests
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(2, 64)))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 64)))
            .mount(&server)
            .await;

        let policy = RetryPolicy {
            batch_size: 2,
            max_attempts: 1,
            initial_backoff: std::time::Duration::from_millis(10),
            request_timeout: std::time::Duration::from_secs(5),
        };
        let embedder = OpenAiEmbedder::new(server.uri(), None, "test-model", 64, None, policy)
            .expect("failed to construct embedder");

        let docs = vec![
            DocumentChunks {
                document_context: "doc1".to_string(),
                chunks: vec!["a".to_string(), "b".to_string()],
            },
            DocumentChunks {
                document_context: "doc2".to_string(),
                chunks: vec!["c".to_string()],
            },
        ];

        let result = embedder.embed_documents(docs).await;
        assert!(
            result.is_ok(),
            "batched embedding should succeed: {result:?}"
        );
        let embedded = result.unwrap();
        assert_eq!(embedded[0].len(), 2);
        assert_eq!(embedded[1].len(), 1);
    }

    #[test]
    fn openai_embedder_construction_does_not_panic() {
        let retry = RetryPolicy::default();
        let result = OpenAiEmbedder::new(
            "https://api.openai.com",
            None,
            "text-embedding-3-small",
            1536,
            None,
            retry,
        );
        assert!(
            result.is_ok(),
            "should be able to construct embedder: {:?}",
            result.err()
        );
    }
}
