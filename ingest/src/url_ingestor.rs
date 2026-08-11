//! URL ingestor: fetches URLs, parses them, and emits typed [`Resource`]s.
//!
//! The CLI's concrete [`Ingestor`] for `url`-kind sources (issue #117). See
//! `file_ingestor` module docs for the general rationale for keeping
//! acquisition I/O out of `core`.

use localdb_core::block::IngestorKind;
use localdb_core::error::Error;
use localdb_core::ingestion::UrlFetcher;
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor, SkipReason};
use localdb_core::parser::Parser;
use localdb_core::uri::Uri;

use crate::url_pipeline::{process_url, ResourceEnrichment, UrlOutcome};

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
        let enrichment = ResourceEnrichment::default();

        for (url, uri) in &urls {
            let outcome = process_url(
                self.parser.as_ref(),
                self.fetcher.as_ref(),
                url,
                uri,
                source,
                IngestorKind::Url,
                &enrichment,
                callback,
                &mut result,
            )
            .await?;

            match outcome {
                UrlOutcome::Unsupported => {
                    // `process_url` deliberately does not report
                    // Unsupported — that's the caller's call. `UrlIngestor`
                    // reports it immediately (contrast `FeedIngestor`, which
                    // attempts an embedded-content fallback first).
                    callback.on_skipped(uri, SkipReason::Unsupported).await;
                    result.resources_skipped += 1;
                }
                UrlOutcome::Empty => {
                    // Deliberate behavior change for `url` sources (Codex
                    // review finding F1): a page that fetches 200 but
                    // extracts to empty Markdown used to flow through as an
                    // empty `Resource`, silently erasing any previously
                    // indexed content for this URI and reporting it as
                    // indexed. `process_url` now catches this before
                    // `on_resource` and returns `Empty` without reporting —
                    // `UrlIngestor` reports it here as `SkipReason::Other`
                    // (NOT `Unsupported`): the parser accepted the format
                    // and returned content, it was just empty, which is a
                    // different condition than "no parser handles this
                    // format" and must land in `docs_skipped`, not
                    // `unsupported_format_count` (see
                    // `specs/05-surfaces.md`'s definition of
                    // `unsupported_format`).
                    callback
                        .on_skipped(
                            uri,
                            SkipReason::Other("extraction produced no content".to_string()),
                        )
                        .await;
                    result.resources_skipped += 1;
                }
                UrlOutcome::Blocked => {
                    // Dead code today — `url` sources are handed the
                    // unrestricted fetcher, which never returns `Blocked` —
                    // but `process_url` is shared with `FeedIngestor` and its
                    // contract must hold for every caller. Falling into the
                    // `_` arm below would be silent data loss: the URI would
                    // never be marked seen, and `url` sources (unlike feed
                    // sources) are NOT exempt from the delete-sweep, so the
                    // previously indexed content would be deleted on the
                    // strength of a refusal that says nothing at all about
                    // whether the resource still exists.
                    //
                    // `Other`, not `Unsupported` or `Error`: the format was
                    // never examined, and nothing failed — we declined to
                    // look. That belongs in `docs_skipped`.
                    callback
                        .on_skipped(
                            uri,
                            SkipReason::Other("destination blocked by fetch policy".to_string()),
                        )
                        .await;
                    result.resources_skipped += 1;
                }
                UrlOutcome::Gone => {
                    // Confirmed 404/410 after retry: the origin was reached
                    // and told us the resource is gone. Report it positively
                    // rather than relying on silence — since #156, an absent
                    // URI is only swept when the run's absences are
                    // trustworthy, and "I know this is deleted" must not be
                    // expressed the same way as "I couldn't look."
                    callback.on_gone(uri).await;
                }
                _ => {}
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::test_doubles::RecordingCallback;
    use localdb_core::ingestion::{FetchMetadata, FetchResult};
    use localdb_core::parser::{ChainParser, ParsedDocument, Probe};
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
                // Non-paginated: only PDFs carry page offsets (#103).
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
        /// The fetcher's destination policy refused the URL.
        Blocked,
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
                    // This fake models no redirects; a real fetcher's
                    // `None` here means "no redirect information
                    // available", not "definitely no redirect" — see
                    // `FetchResult::Downloaded`'s doc comment.
                    final_url: None,
                }),
                Some(ScriptedOutcome::NotModified) => Ok(FetchResult::NotModified),
                Some(ScriptedOutcome::Gone) => Ok(FetchResult::Gone),
                Some(ScriptedOutcome::Blocked) => Ok(FetchResult::Blocked),
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

    /// Issue #187 review finding 2: `url_pipeline::process_url` guards its
    /// `parser.parse(&probe)` call with `localdb_core::run_blocking`, which
    /// only takes the `block_in_place` branch on a multi-thread tokio
    /// runtime — every other test in this module runs on the default
    /// current-thread `#[tokio::test]` runtime and never exercises it. This
    /// forces `flavor = "multi_thread"` so a real end-to-end URL ingestion
    /// actually drives `block_in_place`, proving the call site doesn't
    /// panic there.
    #[tokio::test(flavor = "multi_thread")]
    async fn downloaded_on_multi_thread_runtime_exercises_block_in_place_guard() {
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

    /// Codex review finding F1: a page that fetches 200 but extracts to
    /// empty Markdown must not flow through as an empty `Resource` (which
    /// would silently delete any previously indexed content for the URI —
    /// see `core::ingestion::index_resource`'s empty-chunks arm). It must be
    /// reported as `SkipReason::Other`, NOT `SkipReason::Unsupported`: the
    /// parser accepted the format and returned content, it was just empty —
    /// a different condition from "no parser handles this format", and the
    /// two feed different counters (`docs_skipped` vs
    /// `unsupported_format_count`) that the CLI reports separately.
    #[tokio::test]
    async fn empty_extraction_is_skipped_as_other_not_unsupported() {
        let mut script = HashMap::new();
        script.insert(
            "https://example.com/empty".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: Vec::new(),
                content_type: None,
            },
        );

        let ingestor =
            UrlIngestor::new(Box::new(AllParser), Box::new(ScriptedFetcher::new(script)));
        let source = source_with_urls(&["https://example.com/empty"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(
            result.resources_produced, 0,
            "an empty extraction must never be indexed"
        );
        assert!(
            cb.resources.is_empty(),
            "no Resource must ever be produced for an empty extraction"
        );
        assert_eq!(result.resources_skipped, 1);
        assert_eq!(result.errors, 0, "an empty extraction is not an error");
        assert_eq!(cb.skipped.len(), 1);
        assert_eq!(cb.skipped[0].0, "https://example.com/empty");
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Other(_)),
            "an empty extraction must report SkipReason::Other (docs_skipped), \
             not SkipReason::Unsupported (unsupported_format_count) — got: {:?}",
            cb.skipped[0].1
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

    /// Full-struct `Resource` equality, pinning the `process_url` refactor
    /// (issue #116, `url_pipeline` extraction) as behavior-preserving: every
    /// field must match the exact `Resource` `UrlIngestor` produced before
    /// the extraction, including `external_id: None`, `external_etag: None`,
    /// and `added_at == modified_at` (both the same `now_rfc3339()` call).
    #[tokio::test]
    async fn resource_full_struct_equality_pins_pre_refactor_shape() {
        use localdb_core::block::{Resource, ResourceKind};
        use localdb_core::markdown_blocks::{compute_blocks_hash, markdown_to_blocks};
        use localdb_core::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};

        let content = b"# Pinned Title\n\nPinned body text.\n".to_vec();
        let mut script = HashMap::new();
        script.insert(
            "https://example.com/pinned".to_string(),
            ScriptedOutcome::Downloaded {
                bytes: content,
                content_type: Some("text/markdown".to_string()),
            },
        );

        let ingestor =
            UrlIngestor::new(Box::new(AllParser), Box::new(ScriptedFetcher::new(script)));
        let source = IngestSource {
            policy_version: "policy-pin".to_string(),
            source_id: "src-pin".to_string(),
            store_id: "store-pin".to_string(),
            ingestor_kind: IngestorKind::Url,
            config: serde_json::json!({"url": "https://example.com/pinned"}),
        };
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();
        assert_eq!(result.resources_produced, 1);
        assert_eq!(cb.resources.len(), 1);

        let markdown = "# Pinned Title\n\nPinned body text.\n";
        let blocks = markdown_to_blocks(markdown);
        let hash = compute_blocks_hash(&blocks);
        let uri = Uri::parse("https://example.com/pinned").unwrap();
        let expected = Resource {
            id: localdb_core::ids::resource_id("https://example.com/pinned", &hash),
            store_id: "store-pin".to_string(),
            source_id: "src-pin".to_string(),
            ingestor_kind: IngestorKind::Url,
            resource_kind: ResourceKind::Document,
            uri,
            external_id: None,
            external_etag: None,
            content_hash: hash,
            title: None,
            mime: Some("text/markdown".to_string()),
            metadata: Metadata::Document(DocumentMetadata {
                dublin_core: DublinCoreMetadata::default(),
                ..Default::default()
            }),
            added_at: localdb_core::ingestion::now_rfc3339(),
            modified_at: localdb_core::ingestion::now_rfc3339(),
            thread_id: None,
            channel: None,
            participants: vec![],
            origin_store: "store-pin".to_string(),
            policy_version: "policy-pin".to_string(),
            share_path: None,
            extractor_version: "1.0".to_string(),
            blocks,
        };

        assert_eq!(cb.resources[0], expected);
        assert_eq!(
            cb.resources[0].added_at, cb.resources[0].modified_at,
            "added_at and modified_at must be the same now_rfc3339() string"
        );
    }

    /// `url` sources are NOT exempt from the delete-sweep, so `Blocked` must
    /// be reported rather than falling into the `_` wildcard: an unreported
    /// URI is deleted by the sweep, which would turn a "we declined to
    /// connect" into data loss. Dead code for `url` sources today (they use
    /// the unrestricted fetcher) — this pins `process_url`'s shared contract.
    #[tokio::test]
    async fn blocked_destination_is_reported_not_swallowed() {
        let mut script = HashMap::new();
        script.insert(
            "http://169.254.169.254/latest/meta-data/".to_string(),
            ScriptedOutcome::Blocked,
        );
        let ingestor =
            UrlIngestor::new(Box::new(AllParser), Box::new(ScriptedFetcher::new(script)));
        let source = source_with_urls(&["http://169.254.169.254/latest/meta-data/"]);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_skipped, 1);
        assert_eq!(result.errors, 0, "a policy refusal is not a failure");
        assert!(cb.resources.is_empty());
        assert_eq!(
            cb.skipped.len(),
            1,
            "the URI must be reported so the delete-sweep leaves its content alone"
        );
        assert!(
            matches!(cb.skipped[0].1, SkipReason::Other(_)),
            "expected SkipReason::Other, got {:?}",
            cb.skipped[0].1
        );
    }
}
