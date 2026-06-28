use async_trait::async_trait;
use localdb_core::{
    error::Error,
    ingestion::{FetchMetadata, FetchResult, UrlFetcher},
};
use reqwest::{Client, StatusCode};

/// HTTP URL fetcher backed by reqwest.
pub struct HttpUrlFetcher {
    client: Client,
}

impl HttpUrlFetcher {
    pub fn new() -> Result<Self, Error> {
        let client = Client::builder()
            .user_agent("localdb/0.1")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| Error::ProviderUnavailable {
                message: format!("failed to build HTTP client: {e}"),
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl UrlFetcher for HttpUrlFetcher {
    async fn fetch(&self, url: &str, metadata: &FetchMetadata) -> Result<FetchResult, Error> {
        let mut req = self.client.get(url);

        if let Some(etag) = &metadata.etag {
            req = req.header("If-None-Match", etag);
        }
        if let Some(last_modified) = &metadata.last_modified {
            req = req.header("If-Modified-Since", last_modified);
        }

        let response = req.send().await.map_err(|e| Error::ProviderUnavailable {
            message: format!("HTTP request failed: {e}"),
        })?;

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
        let result = fetcher
            .fetch(&format!("{}/doc", server.uri()), &FetchMetadata::default())
            .await
            .unwrap();

        match result {
            FetchResult::Downloaded {
                bytes,
                content_type,
                etag,
                last_modified,
            } => {
                assert_eq!(bytes, b"hello world");
                assert_eq!(content_type.as_deref(), Some("text/plain"));
                assert_eq!(etag.as_deref(), Some("\"abc123\""));
                assert_eq!(
                    last_modified.as_deref(),
                    Some("Wed, 21 Oct 2025 07:28:00 GMT")
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
}
