//! URL ingestor: fetches URLs, parses them, and emits typed [`Resource`]s.
//!
//! Full-parity port of `core::ingestion::run_url_source` onto the
//! [`Ingestor`] trait (issue #117). See `file_ingestor` module docs for the
//! general porting rationale; deviations from `run_url_source` are called
//! out inline.

use extract::sniff_mime;
use localdb_core::block::{IngestorKind, Resource, ResourceKind};
use localdb_core::error::Error;
use localdb_core::ids::resource_id;
use localdb_core::ingestion::{now_rfc3339, FetchMetadata, FetchResult, UrlFetcher};
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor, SkipReason};
use localdb_core::markdown_blocks::{compute_blocks_hash, markdown_to_blocks};
use localdb_core::metadata::{DocumentMetadata, Metadata};
use localdb_core::parser::{Parser, Probe};
use localdb_core::uri::Uri;

use crate::support::catch_panic;

/// URL ingestor.
///
/// Fetches a list of URLs from `source.config["urls"]` (array of strings).
/// Optionally supports a single URL via `source.config["url"]`. This is a
/// superset of `run_url_source`, which only ever drives a single
/// `SourceSpec::Url { url, .. }` per source; the `urls` (plural) config
/// predates this port and is preserved as-is.
pub struct UrlIngestor {
    /// The parser chain for format detection and extraction.
    pub parser: Box<dyn Parser>,
    /// The HTTP fetcher implementation (the `UrlFetcher` seam stays in
    /// `core`; this ingestor is injected with it, mirroring how
    /// `core::ingestors::UrlIngestor` does).
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

        // `run_url_source` never emits a `Discovered` progress event (it only
        // ever drives one URL, so "total" is trivially 1); with `urls`
        // supporting a batch, reporting the batch size here is a reasonable
        // extension of the same contract `FileIngestor` follows.
        callback.on_discovered(urls.len()).await;

        let mut result = IngestResult::default();

        for url in &urls {
            let fetch_meta = FetchMetadata::default();
            // Note: like `run_url_source`, conditional-GET metadata is always
            // the default (no previously-stored ETag/Last-Modified is
            // threaded in here — `run_url_source` has the same
            // known gap, marked with a `TODO` in `core::ingestion`).
            let fetch_result = match self.fetcher.fetch(url, &fetch_meta).await {
                Ok(r) => r,
                Err(e) => {
                    // Deviation from `run_url_source`: that function
                    // propagates a fetch error via `?`, aborting the whole
                    // run — safe there because it only ever handles one URL
                    // per source. This ingestor supports a batch of URLs, so
                    // a single fetch failure is counted and the batch
                    // continues, matching the pre-existing
                    // `core::ingestors::UrlIngestor` behavior and the test
                    // plan's "fetch error -> errors counter" requirement.
                    tracing::warn!(url = %url, "UrlIngestor: fetch error: {}", e);
                    // Report via on_skipped so the delete-sweep keeps this
                    // URL's previously indexed content: a transient network
                    // failure is not evidence the resource is gone (contrast
                    // with FetchResult::Gone below, which stays silent
                    // precisely so the sweep deletes).
                    callback
                        .on_skipped(url, SkipReason::Other(format!("fetch error: {e}")))
                        .await;
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
                    callback.on_skipped(url, SkipReason::Unchanged).await;
                    result.resources_skipped += 1;
                    continue;
                }
                FetchResult::Gone => {
                    // The resource is confirmed absent (404/410 after
                    // retry). Do NOT yield a Resource and do NOT call
                    // `on_skipped`: the pipeline's delete-sweep treats every
                    // URI reported via `on_resource`/`on_skipped` as still
                    // alive, and removes indexed content only for URIs that
                    // were never reported. Staying silent here is what gets
                    // this URI's chunks deleted (specs/01-architecture.md,
                    // ingestion pipeline shape).
                    tracing::info!(url = %url, "UrlIngestor: URL is gone (404/410)");
                    continue;
                }
            };

            let filename = url.split('/').next_back().map(|s| s.to_string());
            // Mirrors `ChainExtractor` (the real `DocumentExtractor`
            // `run_url_source` is wired to in production): `sniff_mime` over
            // bytes+filename feeds the parser chain's `Probe`, not the HTTP
            // `Content-Type` header (which `DocumentExtractor::extract`
            // never receives either).
            let sniffed = sniff_mime(&bytes, filename.as_deref());
            let probe = Probe::new(&bytes, Some(url.as_str()), sniffed.as_deref());

            // Panic-tolerant parsing — see `file_ingestor` for the rationale.
            let parsed =
                match catch_panic(std::panic::AssertUnwindSafe(|| self.parser.parse(&probe))) {
                    Err(panic_msg) => {
                        tracing::warn!(url = %url, "UrlIngestor: parser panicked: {}", panic_msg);
                        callback.on_skipped(url, SkipReason::Other(panic_msg)).await;
                        result.resources_skipped += 1;
                        continue;
                    }
                    Ok(Ok(Some(doc))) => doc,
                    Ok(Ok(None)) => {
                        callback.on_skipped(url, SkipReason::Unsupported).await;
                        result.resources_skipped += 1;
                        continue;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(url = %url, "UrlIngestor: parser error: {}", e);
                        result.errors += 1;
                        continue;
                    }
                };

            let blocks = markdown_to_blocks(&parsed.markdown);
            let hash = compute_blocks_hash(&blocks);
            let res_id = resource_id(url, &hash);
            let now = now_rfc3339();

            // Title merge: same rule as `FileIngestor` / `index_document`.
            let mut dc = parsed.metadata.clone();
            if dc.title.is_none() {
                dc.title = parsed.title.clone();
            }
            let title = dc.title.clone();

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
                // Parity fix vs. the pre-existing `core::ingestors::UrlIngestor`,
                // which hardcoded "v1": stamp the policy version the caller
                // actually requested for this run.
                policy_version: source.policy_version.clone(),
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
    use crate::support::test_doubles::RecordingCallback;
    use localdb_core::parser::{ChainParser, ParsedDocument};
    use std::collections::HashMap;
    use std::sync::Mutex;

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
                metadata: localdb_core::metadata::DublinCoreMetadata::default(),
            }))
        }
    }

    /// What a scripted fetch should return for one URL, without requiring
    /// `FetchResult` (not `Clone`) to be stored directly.
    enum ScriptedOutcome {
        Downloaded {
            bytes: Vec<u8>,
            content_type: Option<String>,
        },
        NotModified,
        Gone,
        FetchError,
    }

    /// A fake `UrlFetcher` scripted per-URL, and recording which URLs were
    /// actually queried (so tests can assert a fetch error doesn't stop the
    /// batch from proceeding to the next URL).
    #[derive(Default)]
    struct ScriptedFetcher {
        script: HashMap<String, ScriptedOutcome>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedFetcher {
        fn new(script: HashMap<String, ScriptedOutcome>) -> Self {
            Self {
                script,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl UrlFetcher for ScriptedFetcher {
        async fn fetch(&self, url: &str, _meta: &FetchMetadata) -> Result<FetchResult, Error> {
            self.calls.lock().unwrap().push(url.to_string());
            match self.script.get(url) {
                Some(ScriptedOutcome::Downloaded {
                    bytes,
                    content_type,
                }) => Ok(FetchResult::Downloaded {
                    bytes: bytes.clone(),
                    content_type: content_type.clone(),
                    etag: None,
                    last_modified: None,
                }),
                Some(ScriptedOutcome::NotModified) => Ok(FetchResult::NotModified),
                Some(ScriptedOutcome::Gone) => Ok(FetchResult::Gone),
                Some(ScriptedOutcome::FetchError) | None => Err(Error::Internal {
                    message: "simulated fetch error".to_string(),
                    correlation_id: "test_fetch_error".to_string(),
                }),
            }
        }
    }

    fn source_with_urls(urls: &[&str]) -> IngestSource {
        IngestSource {
            policy_version: "policy-xyz".to_string(),
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::Url,
            config: serde_json::json!({"urls": urls}),
        }
    }

    #[tokio::test]
    async fn missing_url_errors() {
        let ingestor = UrlIngestor::new(
            Box::new(ChainParser::new("chain", vec![])),
            Box::new(ScriptedFetcher::default()),
        );
        let source = IngestSource {
            policy_version: "test-policy".to_string(),
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::Url,
            config: serde_json::json!({}),
        };
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await;
        assert!(result.is_err(), "missing url should error");
    }

    #[tokio::test]
    async fn downloaded_produces_resource_with_policy_version_and_mime() {
        let content = b"# Test Page\n\nHello from the web.\n".to_vec();
        let mut script = HashMap::new();
        script.insert(
            "https://example.com/ok".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: content,
                content_type: Some("text/markdown".to_string()),
            },
        );

        let ingestor =
            UrlIngestor::new(Box::new(AllParser), Box::new(ScriptedFetcher::new(script)));
        let source = source_with_urls(&["https://example.com/ok"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_produced, 1);
        assert_eq!(cb.resources.len(), 1);
        let res = &cb.resources[0];
        assert_eq!(res.ingestor_kind, IngestorKind::Url);
        assert!(!res.blocks.is_empty());
        assert_eq!(res.policy_version, "policy-xyz");
        assert_eq!(res.mime.as_deref(), Some("text/markdown"));
        assert_eq!(cb.discovered, vec![1]);
    }

    #[tokio::test]
    async fn not_modified_is_skipped_as_unchanged() {
        let mut script = HashMap::new();
        script.insert(
            "https://example.com/same".to_string(),
            ScriptedOutcome::NotModified,
        );

        let ingestor =
            UrlIngestor::new(Box::new(AllParser), Box::new(ScriptedFetcher::new(script)));
        let source = source_with_urls(&["https://example.com/same"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_produced, 0);
        assert_eq!(result.resources_skipped, 1);
        assert!(cb.resources.is_empty());
        assert_eq!(
            cb.skipped,
            vec![(
                "https://example.com/same".to_string(),
                SkipReason::Unchanged
            )]
        );
    }

    #[tokio::test]
    async fn gone_yields_no_resource_and_is_not_reported_at_all() {
        let mut script = HashMap::new();
        script.insert(
            "https://example.com/gone".to_string(),
            ScriptedOutcome::Gone,
        );

        let ingestor =
            UrlIngestor::new(Box::new(AllParser), Box::new(ScriptedFetcher::new(script)));
        let source = source_with_urls(&["https://example.com/gone"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_produced, 0);
        assert!(
            cb.resources.is_empty(),
            "a gone URL must not yield a Resource"
        );
        // The delete-sweep treats every URI reported via on_resource or
        // on_skipped as alive; a gone URL must be reported through NEITHER so
        // its previously indexed content gets swept.
        assert!(
            cb.skipped.is_empty(),
            "a gone URL must not be reported via on_skipped, or the delete-sweep would preserve it"
        );
        assert_eq!(result.resources_skipped, 0);
        assert_eq!(result.errors, 0);
    }

    #[tokio::test]
    async fn fetch_error_is_counted_and_batch_continues() {
        let content = b"# OK\n\nBody.\n".to_vec();
        let mut script = HashMap::new();
        script.insert(
            "https://example.com/first".to_string(),
            ScriptedOutcome::FetchError,
        );
        script.insert(
            "https://example.com/second".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: content,
                content_type: None,
            },
        );

        let ingestor =
            UrlIngestor::new(Box::new(AllParser), Box::new(ScriptedFetcher::new(script)));
        let source = source_with_urls(&["https://example.com/first", "https://example.com/second"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.errors, 1, "the first URL's fetch error is counted");
        assert_eq!(
            result.resources_produced, 1,
            "the second URL is still fetched and indexed despite the first URL's error"
        );
    }

    #[tokio::test]
    async fn unsupported_format_is_skipped_with_reason() {
        struct NoneParser;
        impl Parser for NoneParser {
            fn id(&self) -> &'static str {
                "none"
            }
            fn parse(&self, _probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
                Ok(None)
            }
        }

        let mut script = HashMap::new();
        script.insert(
            "https://example.com/unsupported".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: b"binary".to_vec(),
                content_type: None,
            },
        );

        let ingestor =
            UrlIngestor::new(Box::new(NoneParser), Box::new(ScriptedFetcher::new(script)));
        let source = source_with_urls(&["https://example.com/unsupported"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_skipped, 1);
        assert_eq!(
            cb.skipped,
            vec![(
                "https://example.com/unsupported".to_string(),
                SkipReason::Unsupported
            )]
        );
    }

    #[tokio::test]
    async fn panicking_parser_is_skipped_not_crashed() {
        struct PanickingParser;
        impl Parser for PanickingParser {
            fn id(&self) -> &'static str {
                "panicking"
            }
            fn parse(&self, _probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
                panic!("simulated parser panic");
            }
        }

        let mut script = HashMap::new();
        script.insert(
            "https://example.com/boom".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: b"whatever".to_vec(),
                content_type: None,
            },
        );

        let ingestor = UrlIngestor::new(
            Box::new(PanickingParser),
            Box::new(ScriptedFetcher::new(script)),
        );
        let source = source_with_urls(&["https://example.com/boom"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_skipped, 1);
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Other(msg) if msg.contains("simulated parser panic"))
        );
    }
}
