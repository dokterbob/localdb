//! URL ingestor: fetches URLs, parses them, and emits typed [`Resource`]s.
//!
//! The CLI's concrete [`Ingestor`] for `url`-kind sources (issue #117). See
//! `file_ingestor` module docs for the general rationale for keeping
//! acquisition I/O out of `core`.

use extract::sniff_mime;
use localdb_core::block::{IngestorKind, Resource, ResourceKind};
use localdb_core::error::Error;
use localdb_core::ids::resource_id;
use localdb_core::ingestion::{now_rfc3339, FetchMetadata, FetchResult, UrlFetcher};
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor, SkipReason};
use localdb_core::markdown_blocks::{compute_blocks_hash, markdown_to_blocks_with_pages};
use localdb_core::metadata::{DocumentMetadata, Metadata};
use localdb_core::parser::{Parser, Probe};
use localdb_core::uri::Uri;

use crate::support::catch_panic;

/// URL ingestor.
///
/// Fetches a list of URLs from `source.config["urls"]` (array of strings).
/// Optionally supports a single URL via `source.config["url"]`. A `Source`'s
/// `SourceSpec::Url { url, .. }` only ever carries a single URL; `urls`
/// (plural) is a superset extension this ingestor also accepts.
pub struct UrlIngestor {
    /// The parser chain for format detection and extraction.
    pub parser: Box<dyn Parser>,
    /// The HTTP fetcher implementation (the `UrlFetcher` seam stays in
    /// `core`; this ingestor is injected with it).
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

        // Parse every configured URL into a canonical `Uri` up front, before
        // any fetching starts. `on_skipped` now takes `&Uri` (core owns
        // identity/normalization; see `Ingestor::on_skipped`'s doc comment),
        // and every skip site below as well as the resource-construction
        // site need one — hoisting this here means each URL is parsed
        // exactly once and shared by all of them, and an unparseable config
        // URL fails the whole run fast (a single `Error::Internal` before
        // any I/O), rather than surfacing deep in the loop only once that
        // particular URL's turn came up.
        let urls: Vec<(String, Uri)> = urls
            .into_iter()
            .map(|u| {
                let uri = Uri::parse(&u).ok_or_else(|| Error::Internal {
                    message: format!("UrlIngestor: invalid URI '{}'", u),
                    correlation_id: "url_ingestor_uri".to_string(),
                })?;
                Ok((u, uri))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        // Report the batch size as `Discovered`, matching the contract
        // `FileIngestor` follows (total known after enumeration).
        callback.on_discovered(urls.len()).await;

        let mut result = IngestResult::default();

        for (url, uri) in &urls {
            let fetch_meta = FetchMetadata::default();
            // Note: conditional-GET metadata is always the default here (no
            // previously-stored ETag/Last-Modified is threaded in) — a known
            // gap, marked with a `TODO` in `core::ingestion`.
            let fetch_result = match self.fetcher.fetch(url, &fetch_meta).await {
                Ok(r) => r,
                Err(e) => {
                    // A single fetch failure is counted and the batch
                    // continues rather than aborting the whole run, per the
                    // test plan's "fetch error -> errors counter" requirement.
                    tracing::warn!(url = %url, "UrlIngestor: fetch error: {}", e);
                    // Report via on_skipped so the delete-sweep keeps this
                    // URL's previously indexed content: a transient network
                    // failure is not evidence the resource is gone (contrast
                    // with FetchResult::Gone below, which stays silent
                    // precisely so the sweep deletes). SkipReason::Error (not
                    // Other) so the pipeline counts this as an error rather
                    // than a benign skip (C8).
                    callback
                        .on_skipped(uri, SkipReason::Error(format!("fetch error: {e}")))
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
                    callback.on_skipped(uri, SkipReason::Unchanged).await;
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
            // `sniff_mime` over bytes+filename feeds the parser chain's
            // `Probe`, not the HTTP `Content-Type` header (the parser chain
            // never receives that header either).
            let sniffed = sniff_mime(&bytes, filename.as_deref());
            let probe = Probe::new(&bytes, Some(url.as_str()), sniffed.as_deref());

            // Panic-tolerant parsing — see `file_ingestor` for the rationale.
            // A panic IS an error (C8, matching the old pipeline's behavior
            // of folding panics into the error count), so it's reported via
            // SkipReason::Error rather than the benign-skip counter.
            let parsed =
                match catch_panic(std::panic::AssertUnwindSafe(|| self.parser.parse(&probe))) {
                    Err(panic_msg) => {
                        tracing::warn!(url = %url, "UrlIngestor: parser panicked: {}", panic_msg);
                        callback.on_skipped(uri, SkipReason::Error(panic_msg)).await;
                        result.errors += 1;
                        continue;
                    }
                    Ok(Ok(Some(doc))) => doc,
                    Ok(Ok(None)) => {
                        callback.on_skipped(uri, SkipReason::Unsupported).await;
                        result.resources_skipped += 1;
                        continue;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(url = %url, "UrlIngestor: parser error: {}", e);
                        // C7: this arm previously never called on_skipped at
                        // all, silently orphaning the URL from the
                        // delete-sweep's "seen" set — a transient parser
                        // failure would erase the URL's previously indexed
                        // chunks. Mirror FileIngestor's aliveness rule.
                        callback
                            .on_skipped(uri, SkipReason::Error(format!("parser error: {e}")))
                            .await;
                        result.errors += 1;
                        continue;
                    }
                };

            // Page stamping (#103): `page_starts` is empty for non-paginated
            // formats, in which case this is plain `markdown_to_blocks`.
            let blocks = markdown_to_blocks_with_pages(&parsed.markdown, &parsed.page_starts);
            let hash = compute_blocks_hash(&blocks);
            let res_id = resource_id(url, &hash);
            let now = now_rfc3339();

            // Title merge: same rule as `FileIngestor` applies.
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
                uri: uri.clone(),
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
                // Stamp the policy version the caller actually requested for
                // this run (not a hardcoded placeholder).
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
                page_starts: Vec::new(),
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

    /// `Ingestor::on_skipped` now takes `&Uri`, so `UrlIngestor` must hold a
    /// canonical `Uri` for every configured URL before it can report
    /// anything through the callback. This test pins the resulting
    /// fail-fast contract: an unparseable config URL is rejected by the
    /// `Uri::parse` hoisted to the top of `ingest()`, before any network
    /// I/O — not discovered lazily once that URL's turn comes up in the
    /// loop. A well-formed URL earlier in the batch does not get fetched
    /// either: the whole run fails as one unit, which is the direct
    /// replacement for the old core-level
    /// `on_skipped_unparseable_locator_falls_back_without_panic` test (that
    /// test's raw-string fallback path is unreachable now that
    /// `on_skipped` no longer accepts anything but an already-valid `Uri`).
    #[tokio::test]
    async fn invalid_config_url_fails_fast() {
        let content = b"# OK\n\nBody.\n".to_vec();
        let mut script = HashMap::new();
        script.insert(
            "https://example.com/ok".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: content,
                content_type: None,
            },
        );
        let fetcher = ScriptedFetcher::new(script);

        let ingestor = UrlIngestor::new(Box::new(AllParser), Box::new(fetcher));
        // The first URL is well-formed; the second is not a parseable URI at
        // all ("not a valid uri" has no scheme).
        let source = source_with_urls(&["https://example.com/ok", "not a valid uri"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await;

        assert!(
            result.is_err(),
            "an unparseable config URL must fail the whole run, not just its own entry"
        );
        assert!(
            cb.resources.is_empty(),
            "fail-fast happens before any URL is fetched or yielded"
        );
        assert!(
            cb.skipped.is_empty(),
            "fail-fast happens before any on_skipped call, including for the good URL"
        );
        assert!(
            cb.discovered.is_empty(),
            "fail-fast happens before on_discovered — no URL is ever queried"
        );
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
        assert_eq!(cb.skipped.len(), 1);
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Error(msg) if msg.contains("fetch error")),
            "a fetch error must report SkipReason::Error (not Other) so the pipeline \
             counts it as an error rather than a benign skip; got: {:?}",
            cb.skipped[0].1
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

        // C8: a parser panic is an error, not a benign skip — count it via
        // `errors`/`SkipReason::Error`, matching `FileIngestor`.
        assert_eq!(result.resources_skipped, 0);
        assert_eq!(result.errors, 1);
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Error(msg) if msg.contains("simulated parser panic"))
        );
    }

    /// C7: a parser returning `Err(...)` (as opposed to panicking) must also
    /// report `on_skipped(SkipReason::Error)` — previously this arm only did
    /// `result.errors += 1; continue;` with no callback call at all, which
    /// silently orphaned the URL from the delete-sweep's "seen" set and
    /// caused a transient parser failure to erase the URL's previously
    /// indexed chunks.
    #[tokio::test]
    async fn parser_error_reports_on_skipped_error_and_stays_alive() {
        struct FailingParser;
        impl Parser for FailingParser {
            fn id(&self) -> &'static str {
                "failing"
            }
            fn parse(&self, _probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
                Err(Error::Internal {
                    message: "simulated parser error".to_string(),
                    correlation_id: "test_parser_error".to_string(),
                })
            }
        }

        let mut script = HashMap::new();
        script.insert(
            "https://example.com/bad-parse".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: b"whatever".to_vec(),
                content_type: None,
            },
        );

        let ingestor = UrlIngestor::new(
            Box::new(FailingParser),
            Box::new(ScriptedFetcher::new(script)),
        );
        let source = source_with_urls(&["https://example.com/bad-parse"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.errors, 1);
        assert_eq!(
            cb.skipped.len(),
            1,
            "the parser error must be reported via on_skipped"
        );
        assert_eq!(cb.skipped[0].0, "https://example.com/bad-parse");
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Error(msg) if msg.contains("simulated parser error")),
            "expected SkipReason::Error mentioning the parser error, got: {:?}",
            cb.skipped[0].1
        );
    }
}
