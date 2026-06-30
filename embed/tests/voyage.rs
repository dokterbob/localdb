use embed::{RetryPolicy, VoyageEmbedder};
use localdb_core::{DocumentChunks, Embedder};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_response(n: usize, dim: usize) -> serde_json::Value {
    let data: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            serde_json::json!({
                "embedding": vec![0.3f32; dim],
                "index": i
            })
        })
        .collect();
    serde_json::json!({ "data": data })
}

fn make_embedder(server_uri: &str) -> VoyageEmbedder {
    VoyageEmbedder::new(
        "voyage-test-key",
        None,
        Some(1024),
        RetryPolicy {
            max_attempts: 3,
            initial_backoff: std::time::Duration::from_millis(10),
            request_timeout: std::time::Duration::from_secs(5),
            batch_size: 32,
        },
    )
    .expect("failed to construct embedder")
    .with_base_url(server_uri)
}

#[tokio::test]
async fn voyage_embedder_correct_shape() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/contextual_embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response(3, 1024)))
        .mount(&server)
        .await;

    let embedder = make_embedder(&server.uri());
    let docs = vec![DocumentChunks {
        document_context: "Full document text for context".to_string(),
        chunks: vec![
            "chunk one".to_string(),
            "chunk two".to_string(),
            "chunk three".to_string(),
        ],
    }];

    let result = embedder.embed_documents(docs).await.unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].len(), 3);
    assert_eq!(result[0][0].len(), 1024);
}

#[tokio::test]
async fn voyage_embedder_retries_on_429() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/contextual_embeddings"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/contextual_embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 1024)))
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
async fn voyage_embedder_fails_after_max_retries() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/contextual_embeddings"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let embedder = VoyageEmbedder::new(
        "key",
        None,
        Some(1024),
        RetryPolicy {
            max_attempts: 2,
            initial_backoff: std::time::Duration::from_millis(10),
            request_timeout: std::time::Duration::from_secs(5),
            batch_size: 32,
        },
    )
    .expect("failed to construct embedder")
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
async fn voyage_embedder_passes_document_field() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/contextual_embeddings"))
        .and(body_partial_json(serde_json::json!({
            "document": "document context text"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(make_response(1, 1024)))
        .mount(&server)
        .await;

    let embedder = make_embedder(&server.uri());
    let docs = vec![DocumentChunks {
        document_context: "document context text".to_string(),
        chunks: vec!["chunk text".to_string()],
    }];

    let result = embedder.embed_documents(docs).await;
    assert!(
        result.is_ok(),
        "voyage contextualized request should succeed: {result:?}"
    );
}

#[tokio::test]
async fn voyage_embedder_timeout_returns_provider_unavailable() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/contextual_embeddings"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(make_response(1, 1024))
                .set_delay(std::time::Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let embedder = VoyageEmbedder::new(
        "key",
        None,
        Some(1024),
        RetryPolicy {
            max_attempts: 1,
            initial_backoff: std::time::Duration::from_millis(10),
            request_timeout: std::time::Duration::from_millis(50),
            batch_size: 32,
        },
    )
    .expect("failed to construct embedder")
    .with_base_url(server.uri());

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
async fn voyage_embedder_empty_docs() {
    let server = MockServer::start().await;
    let embedder = make_embedder(&server.uri());
    let result = embedder.embed_documents(vec![]).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn voyage_embedder_model_id() {
    let embedder = VoyageEmbedder::new("key", None, None, RetryPolicy::default())
        .expect("failed to construct embedder");
    assert_eq!(embedder.model_id(), "voyage-context-3");
    assert_eq!(embedder.embedding_dim(), 1024);
}

#[test]
fn voyage_embedder_construction_does_not_panic() {
    let retry = RetryPolicy::default();
    let result = VoyageEmbedder::new("test-api-key", None, None, retry);
    assert!(
        result.is_ok(),
        "should be able to construct embedder: {:?}",
        result.err()
    );
}
