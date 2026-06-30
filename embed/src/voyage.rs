//! Voyage contextualized embedding provider.
//!
//! Uses Voyage AI's `voyage-context-3` model endpoint, which supports contextualized
//! embeddings by accepting a document alongside the chunks.
//!
//! # API shape (Voyage AI contextual embeddings)
//!
//! Request to `https://api.voyageai.com/v1/contextual_embeddings`:
//! ```json
//! {
//!   "model": "voyage-context-3",
//!   "document": "full document text",
//!   "input": ["chunk 1", "chunk 2", ...]
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
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::EmbedError;
use crate::http_helper::send_with_retry;
use crate::retry::RetryPolicy;

const DEFAULT_BASE_URL: &str = "https://api.voyageai.com";
const DEFAULT_MODEL: &str = "voyage-context-3";
const DEFAULT_DIM: usize = 1024;

/// Request body for Voyage `/v1/contextual_embeddings`.
#[derive(Debug, Serialize)]
struct VoyageEmbedRequest<'a> {
    model: &'a str,
    document: &'a str,
    input: &'a [String],
}

/// One embedding object in the Voyage response.
#[derive(Debug, Deserialize)]
struct VoyageEmbeddingObject {
    embedding: Vec<f32>,
    index: usize,
}

/// Response from Voyage `/v1/contextual_embeddings`.
#[derive(Debug, Deserialize)]
struct VoyageEmbedResponse {
    data: Vec<VoyageEmbeddingObject>,
}

/// Voyage contextualized embedding provider.
///
/// Uses the `voyage-context-3` model with document-level context.
pub struct VoyageEmbedder {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    embedding_dim: usize,
    retry: RetryPolicy,
}

impl VoyageEmbedder {
    /// Create a new Voyage embedder.
    pub fn new(
        api_key: impl Into<String>,
        model: Option<String>,
        embedding_dim: Option<usize>,
        retry: RetryPolicy,
    ) -> Result<Self, EmbedError> {
        let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let embedding_dim = embedding_dim.unwrap_or(DEFAULT_DIM);
        let client = Client::builder()
            .timeout(retry.request_timeout)
            .build()
            .map_err(|e| EmbedError::Internal(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            model,
            embedding_dim,
            retry,
        })
    }

    /// Create from environment variable for the API key.
    pub fn from_env(api_key_env: &str) -> Option<Result<Self, EmbedError>> {
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
            "{}/v1/contextual_embeddings",
            self.base_url.trim_end_matches('/')
        );

        let body = VoyageEmbedRequest {
            model: &self.model,
            document: document_context,
            input: chunks,
        };
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|e| {
                EmbedError::Internal(format!("failed to build authorization header: {e}"))
            })?,
        );

        let response = send_with_retry(
            &self.client,
            &url,
            headers,
            serde_json::to_vec(&body).map_err(|e| EmbedError::ProviderError {
                provider: "voyage".to_string(),
                message: format!("failed to serialize request: {e}"),
            })?,
            &self.retry,
        )
        .await?;

        let resp: VoyageEmbedResponse =
            serde_json::from_slice(&response).map_err(|e| EmbedError::ProviderError {
                provider: "voyage".to_string(),
                message: format!("failed to parse response: {e}"),
            })?;

        let mut vecs: Vec<Option<Vec<f32>>> = vec![None; chunks.len()];
        for obj in resp.data {
            if obj.index < vecs.len() {
                vecs[obj.index] = Some(obj.embedding);
            }
        }
        let result: Option<Vec<Vec<f32>>> = vecs.into_iter().collect();
        result.ok_or_else(|| EmbedError::ProviderError {
            provider: "voyage".to_string(),
            message: "response missing some embedding indices".to_string(),
        })
    }
}

#[async_trait]
impl Embedder for VoyageEmbedder {
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
