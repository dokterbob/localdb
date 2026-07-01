//! URL ingestor: fetches URLs, parses them, and emits typed [`Resource`]s.
//!
//! The [`UrlIngestor`] implements [`Ingestor`] for `IngestorKind::Url`.  It
//! uses the [`UrlFetcher`] trait from `crate::ingestion` for HTTP fetching and
//! the `Parser` trait for format-specific parsing.

use crate::block::{IngestorKind, Resource, ResourceKind};
use crate::error::Error;
use crate::ids::document_id;
use crate::ingestion::{now_rfc3339, FetchMetadata, FetchResult, UrlFetcher};
use crate::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor};
use crate::ingestors::file_ingestor::parser_meta_to_dc;
use crate::markdown_blocks::{compute_blocks_hash, markdown_to_blocks};
use crate::metadata::{DocumentMetadata, Metadata};
use crate::parser::{Parser, Probe};
use crate::uri::Uri;

/// URL ingestor.
///
/// Fetches a list of URLs from `source.config["urls"]` (array of strings).
/// Optionally supports a single URL via `source.config["url"]`.
pub struct UrlIngestor {
    /// The parser chain for format detection and extraction.
    pub parser: Box<dyn Parser>,
    /// The HTTP fetcher implementation.
    pub fetcher: Box<dyn UrlFetcher>,
}

impl UrlIngestor {
    /// Create a new `UrlIngestor` with the given parser chain and fetcher.
    pub fn new(parser: Box<dyn Parser>, fetcher: Box<dyn UrlFetcher>) -> Self {
        Self { parser, fetcher }
    }
}

#[async_trait::async_trait]
impl Ingestor for UrlIngestor {
    fn kind(&self) -> IngestorKind {
        IngestorKind::Url
    }

    async fn ingest(
        &self,
        source: &IngestSource,
        callback: &mut dyn IngestCallback,
    ) -> Result<IngestResult, Error> {
        // Collect URLs from config.
        let mut urls: Vec<String> = Vec::new();

        // Support both "url" (single string) and "urls" (array).
        if let Some(u) = source.config.get("url").and_then(|v| v.as_str()) {
            urls.push(u.to_string());
        }
        if let Some(arr) = source.config.get("urls").and_then(|v| v.as_array()) {
            for v in arr {
                if let Some(u) = v.as_str() {
                    urls.push(u.to_string());
                }
            }
        }

        if urls.is_empty() {
            return Err(Error::InvalidRequest {
                message: "UrlIngestor: missing required config field 'url' or 'urls'".to_string(),
            });
        }

        let mut result = IngestResult::default();

        for url in &urls {
            let fetch_meta = FetchMetadata::default();
            let fetch_result = match self.fetcher.fetch(url, &fetch_meta).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(url = %url, "UrlIngestor: fetch error: {}", e);
                    result.errors += 1;
                    continue;
                }
            };

            let (bytes, content_type) = match fetch_result {
                FetchResult::Downloaded {
                    bytes,
                    content_type,
                    ..
                } => (bytes, content_type),
                FetchResult::NotModified => {
                    result.resources_skipped += 1;
                    continue;
                }
                FetchResult::Gone => {
                    tracing::info!(url = %url, "UrlIngestor: URL is gone (404/410)");
                    result.resources_skipped += 1;
                    continue;
                }
            };

            let probe = Probe::new(&bytes, Some(url.as_str()), content_type.as_deref());

            let parsed = match self.parser.parse(&probe) {
                Ok(Some(doc)) => doc,
                Ok(None) => {
                    result.resources_skipped += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(url = %url, "UrlIngestor: parser error: {}", e);
                    result.errors += 1;
                    continue;
                }
            };

            let blocks = markdown_to_blocks(&parsed.markdown);
            let hash = compute_blocks_hash(&blocks);
            let res_id = document_id(url, &hash);
            let now = now_rfc3339();

            let dc = parser_meta_to_dc(&parsed.metadata);
            let title = parsed.title.clone().or_else(|| dc.title.clone());

            let resource = Resource {
                id: res_id,
                store_id: source.store_id.clone(),
                source_id: source.source_id.clone(),
                ingestor_kind: IngestorKind::Url,
                resource_kind: ResourceKind::Document,
                uri: Uri::parse(url).ok_or_else(|| Error::Internal {
                    message: format!("UrlIngestor: invalid URI '{}'", url),
                    correlation_id: "url_ingestor_uri".to_string(),
                })?,
                external_id: None,
                external_etag: None,
                content_hash: hash,
                title,
                mime: content_type,
                metadata: Metadata::Document(DocumentMetadata {
                    dublin_core: dc,
                    ..Default::default()
                }),
                added_at: now.clone(),
                modified_at: now,
                thread_id: None,
                channel: None,
                participants: vec![],
                origin_store: source.store_id.clone(),
                policy_version: "v1".to_string(),
                share_path: None,
                extractor_version: "1.0".to_string(),
                blocks,
            };

            callback.on_resource(resource).await?;
            result.resources_produced += 1;
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingestion::FetchResult;
    use crate::ingestor::IngestSource;
    use crate::parser::{ChainParser, ParsedDocument};

    struct AllParser;
    impl Parser for AllParser {
        fn id(&self) -> &'static str {
            "all"
        }
        fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
            let text = String::from_utf8_lossy(probe.bytes()).to_string();
            Ok(Some(ParsedDocument {
                markdown: text,
                title: None,
                metadata: crate::parser::DocumentMetadata::default(),
            }))
        }
    }

    struct StaticFetcher {
        content: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl UrlFetcher for StaticFetcher {
        async fn fetch(&self, _url: &str, _meta: &FetchMetadata) -> Result<FetchResult, Error> {
            Ok(FetchResult::Downloaded {
                bytes: self.content.clone(),
                content_type: Some("text/markdown".to_string()),
                etag: None,
                last_modified: None,
            })
        }
    }

    #[tokio::test]
    async fn url_ingestor_missing_url_errors() {
        let ingestor = UrlIngestor::new(
            Box::new(ChainParser::new("chain", vec![])),
            Box::new(StaticFetcher { content: vec![] }),
        );
        let source = IngestSource {
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::Url,
            config: serde_json::json!({}),
        };
        struct NullCallback;
        #[async_trait::async_trait]
        impl IngestCallback for NullCallback {
            async fn on_resource(&mut self, _r: Resource) -> Result<(), Error> {
                Ok(())
            }
        }
        let result = ingestor.ingest(&source, &mut NullCallback).await;
        assert!(result.is_err(), "missing url should error");
    }

    #[tokio::test]
    async fn url_ingestor_fetches_and_produces_resource() {
        let content = b"# Test Page\n\nHello from the web.\n".to_vec();
        let ingestor = UrlIngestor::new(Box::new(AllParser), Box::new(StaticFetcher { content }));
        let source = IngestSource {
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::Url,
            config: serde_json::json!({"url": "https://example.com/test"}),
        };

        struct CollectCallback {
            resources: Vec<Resource>,
        }
        #[async_trait::async_trait]
        impl IngestCallback for CollectCallback {
            async fn on_resource(&mut self, r: Resource) -> Result<(), Error> {
                self.resources.push(r);
                Ok(())
            }
        }
        let mut cb = CollectCallback { resources: vec![] };
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();
        assert_eq!(result.resources_produced, 1);
        assert_eq!(cb.resources.len(), 1);

        let res = &cb.resources[0];
        assert_eq!(res.ingestor_kind, IngestorKind::Url);
        assert!(!res.blocks.is_empty(), "resource should have blocks");
    }
}
