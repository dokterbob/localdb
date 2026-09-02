use super::*;
use crate::support::test_doubles::RecordingCallback;
use localdb_core::ingestion::{FetchMetadata, FetchResult};
use localdb_core::parser::{ParsedDocument, Probe};
use std::collections::HashMap;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

/// Pass-through parser for entry pages fetched in discovery mode: treats
/// bytes as UTF-8 Markdown, no title, empty Dublin Core. Mirrors
/// `url_ingestor::tests::AllParser`.
struct PlainParser;
impl Parser for PlainParser {
    fn id(&self) -> &'static str {
        "plain"
    }
    fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        let text = String::from_utf8_lossy(probe.bytes()).to_string();
        Ok(Some(ParsedDocument {
            markdown: text,
            title: None,
            metadata: DublinCoreMetadata::default(),
            // Non-paginated: only PDFs carry page offsets (#103).
            page_starts: Vec::new(),
        }))
    }
}

/// Declines everything — simulates "no parser supports this format".
struct NoneParser;
impl Parser for NoneParser {
    fn id(&self) -> &'static str {
        "none"
    }
    fn parse(&self, _probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        Ok(None)
    }
}

enum ScriptedOutcome {
    Downloaded {
        bytes: Vec<u8>,
        content_type: Option<String>,
        etag: Option<String>,
        last_modified: Option<String>,
        final_url: Option<String>,
    },
    /// A bare `NotModified` (both fields `None`) is the common case — a
    /// 304 that carried no validators of its own. Non-`None` fields script
    /// a 304 that rotated its `ETag`/`Last-Modified`.
    NotModified {
        etag: Option<String>,
        last_modified: Option<String>,
    },
    Gone,
    /// The destination guard refused this URL — no connection was made.
    Blocked,
}

impl ScriptedOutcome {
    fn text(body: &str) -> Self {
        ScriptedOutcome::Downloaded {
            bytes: body.as_bytes().to_vec(),
            content_type: None,
            etag: None,
            last_modified: None,
            final_url: None,
        }
    }

    fn text_with_etag(body: &str, etag: &str) -> Self {
        ScriptedOutcome::Downloaded {
            bytes: body.as_bytes().to_vec(),
            content_type: None,
            etag: Some(etag.to_string()),
            last_modified: None,
            final_url: None,
        }
    }

    /// Models a 200 whose response carries both conditional-GET validators —
    /// for feed-document capture tests.
    fn text_with_validators(body: &str, etag: &str, last_modified: &str) -> Self {
        ScriptedOutcome::Downloaded {
            bytes: body.as_bytes().to_vec(),
            content_type: None,
            etag: Some(etag.to_string()),
            last_modified: Some(last_modified.to_string()),
            final_url: None,
        }
    }

    /// Models a fetch that followed a redirect: `final_url` is the
    /// post-redirect effective URL, distinct from whatever URL the
    /// `ScriptedFetcher` was keyed on (the pre-redirect, configured URL).
    fn text_redirected_from(body: &str, final_url: &str) -> Self {
        ScriptedOutcome::Downloaded {
            bytes: body.as_bytes().to_vec(),
            content_type: None,
            etag: None,
            last_modified: None,
            final_url: Some(final_url.to_string()),
        }
    }
}

/// Fake `UrlFetcher` scripted per-URL, recording every URL actually queried
/// (so tests can assert e.g. a `<content src=...>` URL is never fetched).
/// Mirrors `url_ingestor::tests::ScriptedFetcher`.
#[derive(Default)]
struct ScriptedFetcher {
    script: HashMap<String, ScriptedOutcome>,
    calls: Mutex<Vec<String>>,
    /// The `FetchMetadata` each call actually received, in call order,
    /// paired with the URL — so a test can assert on what was *sent*, not
    /// just the scripted outcome.
    received: Mutex<Vec<(String, FetchMetadata)>>,
}

impl ScriptedFetcher {
    fn new(script: HashMap<String, ScriptedOutcome>) -> Self {
        Self {
            script,
            calls: Mutex::new(Vec::new()),
            received: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self, url: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|u| *u == url)
            .count()
    }

    /// The most recent `FetchMetadata` sent for `url`, if it was ever
    /// fetched.
    fn last_received_for(&self, url: &str) -> Option<FetchMetadata> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(u, _)| u == url)
            .map(|(_, meta)| meta.clone())
    }
}

#[async_trait::async_trait]
impl UrlFetcher for ScriptedFetcher {
    async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.calls.lock().unwrap().push(url.to_string());
        self.received
            .lock()
            .unwrap()
            .push((url.to_string(), meta.clone()));
        match self.script.get(url) {
            Some(ScriptedOutcome::Downloaded {
                bytes,
                content_type,
                etag,
                last_modified,
                final_url,
            }) => Ok(FetchResult::Downloaded {
                bytes: bytes.clone(),
                content_type: content_type.clone(),
                etag: etag.clone(),
                last_modified: last_modified.clone(),
                final_url: final_url.clone(),
            }),
            Some(ScriptedOutcome::NotModified {
                etag,
                last_modified,
            }) => Ok(FetchResult::NotModified {
                etag: etag.clone(),
                last_modified: last_modified.clone(),
            }),
            Some(ScriptedOutcome::Gone) => Ok(FetchResult::Gone),
            Some(ScriptedOutcome::Blocked) => Ok(FetchResult::Blocked),
            None => Err(Error::Internal {
                message: "simulated fetch error".to_string(),
                correlation_id: "test_fetch_error".to_string(),
            }),
        }
    }
}

fn source_for(feed_url: &str, max_entries: Option<u32>, fetch_full_content: bool) -> IngestSource {
    let mut config = serde_json::json!({
        "url": feed_url,
        "fetch_full_content": fetch_full_content,
    });
    if let Some(m) = max_entries {
        config["max_entries"] = serde_json::json!(m);
    }
    IngestSource {
        policy_version: "policy-1".to_string(),
        source_id: "src-1".to_string(),
        store_id: "store-1".to_string(),
        ingestor_kind: IngestorKind::Feed,
        config,
        document_validators: FetchMetadata::default(),
    }
}

/// Like [`source_for`], but with explicit stored validators for the feed
/// document itself — for tests exercising conditional-GET replay.
fn source_with_validators(
    feed_url: &str,
    fetch_full_content: bool,
    document_validators: FetchMetadata,
) -> IngestSource {
    IngestSource {
        document_validators,
        ..source_for(feed_url, None, fetch_full_content)
    }
}

fn ingestor_with(
    script: HashMap<String, ScriptedOutcome>,
) -> (FeedIngestor, std::sync::Arc<ScriptedFetcher>) {
    let fetcher = std::sync::Arc::new(ScriptedFetcher::new(script));
    // FeedIngestor owns a `Box<dyn UrlFetcher>`; wrap the Arc so the test can
    // still inspect `calls` after construction.
    struct ArcFetcher(std::sync::Arc<ScriptedFetcher>);
    #[async_trait::async_trait]
    impl UrlFetcher for ArcFetcher {
        async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
            self.0.fetch(url, meta).await
        }
    }
    // Both the feed fetcher and the entry fetcher are the same scripted
    // double here: the production split is about *destination policy*, which
    // a scripted fetcher has none of, and sharing one script keeps every
    // pre-existing test's expectations (including `call_count`) unchanged.
    // `feed_entry_link_blocked_falls_back_to_embedded_content` below scripts
    // a `Blocked` outcome to exercise the entry-fetcher path specifically.
    let ingestor = FeedIngestor::new(
        Box::new(PlainParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher.clone())),
    );
    (ingestor, fetcher)
}

// ---------------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------------

fn rss2_feed(channel_extra: &str, items_xml: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><rss version="2.0"><channel><title>Test Feed</title><link>https://feed.example.com/</link><description>Test feed description</description>{channel_extra}{items_xml}</channel></rss>"#
    )
}

fn atom_feed(entries_xml: &str) -> String {
    atom_feed_with("", entries_xml)
}

/// `atom_feed` plus arbitrary feed-level children (e.g. a feed-level
/// `<author>`), injected before the entries.
fn atom_feed_with(feed_extra: &str, entries_xml: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom"><title>Test Feed</title><id>urn:test-feed</id><updated>2026-01-01T00:00:00Z</updated><link href="https://feed.example.com/" rel="alternate"/>{feed_extra}{entries_xml}</feed>"#
    )
}

// ---------------------------------------------------------------------------
// Config / fail-fast
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invalid_config_feed_url_fails_fast_before_discovery() {
    let (ingestor, _fetcher) = ingestor_with(HashMap::new());
    let source = source_for("not a valid uri", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await;

    assert!(
        result.is_err(),
        "an invalid feed URL must fail the whole run"
    );
    assert!(
        cb.discovered.is_empty(),
        "fail-fast happens before on_discovered"
    );
    assert!(cb.resources.is_empty());
    assert!(cb.skipped.is_empty());
}

#[tokio::test]
async fn missing_url_config_errors() {
    let (ingestor, _fetcher) = ingestor_with(HashMap::new());
    let source = IngestSource {
        policy_version: "p".to_string(),
        source_id: "s".to_string(),
        store_id: "st".to_string(),
        ingestor_kind: IngestorKind::Feed,
        config: serde_json::json!({}),
        document_validators: FetchMetadata::default(),
    };
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Feed-level fetch outcomes
// ---------------------------------------------------------------------------

/// Run a feed-document 304 in the given mode and hand back what the
/// callback saw. The two modes disagree about one thing only — whether the
/// feed document is a *document* — so they are worth reading side by side.
async fn feed_level_304_in(fetch_full_content: bool) -> (IngestResult, RecordingCallback) {
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::NotModified {
            etag: None,
            last_modified: None,
        },
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for(
        "https://feed.example.com/feed.xml",
        None,
        fetch_full_content,
    );
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();
    (result, cb)
}

/// In discovery mode the feed document never becomes a `Resource`, so a
/// feed-level 304 must report **no document at all** — not even a skipped
/// one.
///
/// `on_skipped` is not free: it increments `docs_seen` and `docs_skipped`
/// and emits a full `DocumentStarted`/`DocumentFinished` pair, and those
/// counters partition `docs_seen` (specs/04-search-pipeline.md). Reporting
/// the feed document here would therefore count a document that does not
/// exist — and since a healthy feed 304s on every run, that phantom is the
/// steady state, not an edge. It is the same leak the rejected phantom
/// zero-chunk `resources` row would have caused, one layer up.
#[tokio::test]
async fn feed_level_not_modified_in_discovery_mode_reports_no_document() {
    let (result, cb) = feed_level_304_in(true).await;

    assert_eq!(result.resources_produced, 0);
    assert_eq!(result.errors, 0);
    assert_eq!(
        result.resources_skipped, 0,
        "the feed document is not one of this mode's resources"
    );
    assert!(
        cb.discovered.is_empty(),
        "no on_discovered on feed-level 304"
    );
    assert!(cb.resources.is_empty());
    assert!(
        cb.skipped.is_empty(),
        "a discovery-mode feed 304 is a source-level event, not a document one: {:?}",
        cb.skipped
    );
    assert!(
        matches!(result.enumeration, Enumeration::Complete),
        "a 304 is the one zero-callback outcome that DID observe the window — the origin \
         said it is unchanged. Reporting Incomplete here would suppress the feed liveness \
         sweep on every quiet run, which is precisely the case it exists for \
         (specs/04-search-pipeline.md §1 \"Guards\")."
    );
}

/// The other mode, and the reason the check above is conditional rather than
/// an unconditional removal: with `fetch_full_content: false` the feed
/// document *is* the source's one resource, so its 304 is an ordinary
/// unchanged skip and must still be reported as one.
#[tokio::test]
async fn feed_level_not_modified_in_single_document_mode_is_an_unchanged_skip() {
    let (result, cb) = feed_level_304_in(false).await;

    assert_eq!(result.resources_produced, 0);
    assert_eq!(result.errors, 0);
    assert_eq!(result.resources_skipped, 1);
    assert!(cb.resources.is_empty());
    assert_eq!(cb.skipped.len(), 1);
    assert_eq!(cb.skipped[0].1, SkipReason::Unchanged);
    assert_eq!(
        cb.skipped[0].0.as_str(),
        "https://feed.example.com/feed.xml",
        "and it is the feed document itself that is reported"
    );
    assert!(
        matches!(result.enumeration, Enumeration::Complete),
        "a 304 is the one zero-callback outcome that DID observe the window — the origin \
         said it is unchanged. Reporting Incomplete here would suppress the feed liveness \
         sweep on every quiet run, which is precisely the case it exists for \
         (specs/04-search-pipeline.md §1 \"Guards\")."
    );
}

/// A feed-level 404/410 is a complete no-op: zero errors, zero skips, zero
/// produced, and not a single callback.
///
/// The consequence matters as much as the silence. Because
/// `run_source_ingestion` exempts `SourceSpec::Feed` sources from the
/// delete-sweep, this silence does *not* prune anything — whatever the feed
/// indexed previously stays until `source remove`. That is deliberate:
/// dropping a whole source on one 404 is irreversible and a feed-root 404 is
/// often transient (issue #156, "unavailable != empty"). The reclamation
/// policy is tracked in issue #171; see the `FetchResult::Gone` arm in
/// `feed_ingestor.rs` and known gap 8 in docs/architecture.md.
#[tokio::test]
async fn feed_level_gone_is_silent_zero_errors() {
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::Gone,
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.errors, 0);
    assert_eq!(result.resources_produced, 0);
    assert_eq!(
        result.resources_skipped, 0,
        "a gone feed is not reported as a skip either"
    );
    assert!(cb.discovered.is_empty());
    assert!(cb.resources.is_empty());
    assert!(cb.skipped.is_empty(), "Gone must not be reported at all");
}

#[tokio::test]
async fn feed_fetch_error_counts_one_error_and_returns_ok() {
    let script = HashMap::new(); // no entry -> ScriptedFetcher returns FetchError
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.errors, 1);
    assert_eq!(cb.skipped.len(), 1);
    assert!(matches!(&cb.skipped[0].1, SkipReason::Error(_)));
}

#[tokio::test]
async fn malformed_xml_bytes_is_error_not_panic() {
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text("<rss><this is <<not valid xml"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.errors, 1);
    assert_eq!(cb.skipped.len(), 1);
    assert!(matches!(&cb.skipped[0].1, SkipReason::Error(_)));
}

// ---------------------------------------------------------------------------
// Empty feed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_feed_discovery_mode_zero_resources_zero_errors() {
    let feed_xml = rss2_feed("", "");
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.discovered, vec![0]);
    assert_eq!(result.resources_produced, 0);
    assert_eq!(result.errors, 0);
}

#[tokio::test]
async fn empty_feed_single_doc_mode_emits_one_title_only_resource() {
    let feed_xml = rss2_feed("", "");
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.discovered, vec![1]);
    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    assert_eq!(cb.resources[0].title.as_deref(), Some("Test Feed"));
    // No `feed.updated` and no entries at all: single-document mode's
    // `modified_at_override` chain bottoms out at `None` — no claim, never
    // an ingestion-time `now()` stand-in (#283).
    assert_eq!(
        cb.resources[0].modified_at, None,
        "a feed with no updated timestamp and no entries makes no modification-time claim"
    );
}

/// Codex review finding F6: the `feed_rs` parse (`catch_panic(...)` around
/// `feed_rs::parser::Builder::new()....parse(...)`) is CPU-bound and is now
/// guarded with `localdb_core::run_blocking`, mirroring the existing
/// `parser.parse` guard in `file_ingestor.rs` /
/// `discovery_on_multi_thread_runtime_exercises_block_in_place_guard`: this
/// ingestor may run under the daemon's shared multi-thread tokio runtime, and
/// `run_blocking` only takes its `block_in_place` branch there — the default
/// `#[tokio::test]` current-thread runtime never exercises it. This is the
/// first such test in this crate for feed parsing specifically. It does not
/// (and cannot, per the task's stated limitation) prove worker-starvation is
/// avoided — only that the wrapped feed-parse path still behaves correctly on
/// a multi-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn feed_parse_on_multi_thread_runtime_exercises_block_in_place_guard() {
    let feed_xml = rss2_feed("", "");
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.discovered, vec![1]);
    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    assert_eq!(cb.resources[0].title.as_deref(), Some("Test Feed"));
}

// ---------------------------------------------------------------------------
// RSS 2.0: entity-encoded description, rel-less link, discovery mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rss2_entity_encoded_description_and_rel_less_link_discovery() {
    let items = r#"<item><title>Entry One</title><link>https://feed.example.com/e1</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Bold &lt;b&gt;text&lt;/b&gt; and &amp; ampersand</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::text("# Full Page\n\nFull page body.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.discovered, vec![1]);
    assert_eq!(result.resources_produced, 1);
    let res = &cb.resources[0];
    assert_eq!(res.uri.as_str(), "https://feed.example.com/e1");
    assert_eq!(res.external_id.as_deref(), Some("e1"));
    // The rel-less RSS <link> was correctly selected as the entry link (no
    // rel attribute at all -> feed-rs leaves `rel: None`).
}

// ---------------------------------------------------------------------------
// Atom 1.0 discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn atom_feed_discovery_mode_enriches_resource() {
    let entries = r#"<entry><title>Atom Entry</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><published>2026-01-05T00:00:00Z</published><author><name>Jane Doe</name></author><link href="https://feed.example.com/e1" rel="alternate"/><summary>A summary</summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::text_with_etag("# Atom Page\n\nBody.\n", "W/\"etag-123\""),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    let res = &cb.resources[0];
    assert_eq!(res.external_id.as_deref(), Some("urn:e1"));
    assert_eq!(res.external_etag.as_deref(), Some("W/\"etag-123\""));
    assert_eq!(
        res.metadata.dublin_core().creator,
        vec!["Jane Doe".to_string()]
    );
    assert_eq!(
        res.metadata.dublin_core().source.as_deref(),
        Some("https://feed.example.com/feed.xml")
    );
    assert!(res.metadata.dublin_core().date.is_some());
    assert_eq!(res.ingestor_kind, IngestorKind::Feed);
}

// ---------------------------------------------------------------------------
// JSON Feed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_feed_discovery_mode_parses_and_enriches() {
    let feed_json = r#"{
        "version": "https://jsonfeed.org/version/1",
        "title": "JSON Test Feed",
        "items": [
            {
                "id": "https://feed.example.com/e1",
                "url": "https://feed.example.com/e1",
                "title": "JSON Entry",
                "date_published": "2026-01-05T00:00:00Z",
                "author": {"name": "J. Author"},
                "content_html": "<p>inline html</p>"
            }
        ]
    }"#;
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.json".to_string(),
        ScriptedOutcome::text(feed_json),
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::text("# JSON Page\n\nBody.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.json", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    let res = &cb.resources[0];
    assert_eq!(
        res.external_id.as_deref(),
        Some("https://feed.example.com/e1")
    );
    assert_eq!(res.uri.as_str(), "https://feed.example.com/e1");
}

// ---------------------------------------------------------------------------
// Charset handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn iso_8859_1_fixture_decodes_correctly() {
    // Real 0xE9 (é in Latin-1) bytes in a byte-string literal, prolog
    // declares the encoding. feed-rs must decode this itself — never
    // pre-decode with String::from_utf8 (it would fail outright on 0xE9).
    let mut xml: Vec<u8> = Vec::new();
    xml.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>\n");
    xml.extend_from_slice(b"<rss version=\"2.0\"><channel><title>Caf\xe9 Feed</title><link>https://feed.example.com/</link><description>d</description>");
    xml.extend_from_slice(b"<item><title>Entry Caf\xe9</title><link>https://feed.example.com/e1</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body</description></item>");
    xml.extend_from_slice(b"</channel></rss>");

    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::Downloaded {
            bytes: xml,
            content_type: None,
            etag: None,
            last_modified: None,
            final_url: None,
        },
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::text("# Page\n\nBody.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(cb.resources[0].external_id.as_deref(), Some("e1"));
    // Decoding correctness is exercised at the feed-rs layer directly too
    // (see module doc); here we just need the run to succeed at all, which
    // it wouldn't if the bytes were rejected as invalid UTF-8 upstream.
}

#[tokio::test]
async fn windows_1251_fixture_decodes_correctly() {
    let title_bytes = "Заголовок".as_bytes(); // UTF-8 source text
    let win1251: Vec<u8> = encode_windows_1251(std::str::from_utf8(title_bytes).unwrap());

    let mut xml: Vec<u8> = Vec::new();
    xml.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"windows-1251\"?>\n");
    xml.extend_from_slice(b"<rss version=\"2.0\"><channel><title>Feed</title><link>https://feed.example.com/</link><description>d</description>");
    xml.extend_from_slice(b"<item><title>");
    xml.extend_from_slice(&win1251);
    xml.extend_from_slice(b"</title><link>https://feed.example.com/e1</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body</description></item>");
    xml.extend_from_slice(b"</channel></rss>");

    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::Downloaded {
            bytes: xml,
            content_type: None,
            etag: None,
            last_modified: None,
            final_url: None,
        },
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::text("# Page\n\nBody.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
}

/// Minimal windows-1251 encoder for the handful of Cyrillic characters used
/// by the test fixture above (avoids pulling in `encoding_rs` as a direct
/// dev-dependency just for one test). windows-1251 maps U+0410..=U+044F
/// (majority of the Cyrillic alphabet) to the contiguous byte range
/// 0xC0..=0xFF, plus a handful of extra letters outside that block that
/// this helper does not need to support.
fn encode_windows_1251(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| {
            let cp = c as u32;
            if (0x0410..=0x044F).contains(&cp) {
                (cp - 0x0410 + 0xC0) as u8
            } else {
                b'?'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// CDATA vs entity-encoded equivalence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cdata_description_equals_entity_encoded_equivalent() {
    async fn run(description_xml: &str) -> String {
        let items = format!(
            r#"<item><title>E1</title><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>{description_xml}</description></item>"#
        );
        let feed_xml = rss2_feed("", &items);
        let mut script = HashMap::new();
        script.insert(
            "https://feed.example.com/feed.xml".to_string(),
            ScriptedOutcome::text(&feed_xml),
        );
        let (ingestor, _fetcher) = ingestor_with(script);
        // Link-less entry (no <link>) forces the embedded-content fallback,
        // which is what actually routes the description through
        // extract_html — the thing we're pinning here.
        let source = source_for("https://feed.example.com/feed.xml", None, true);
        let mut cb = RecordingCallback::default();
        ingestor.ingest(&source, &mut cb).await.unwrap();
        assert_eq!(cb.resources.len(), 1);
        cb.resources[0]
            .blocks
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    let entity = run("&lt;p&gt;Hello &lt;b&gt;world&lt;/b&gt;&lt;/p&gt;").await;
    let cdata = run("<![CDATA[<p>Hello <b>world</b></p>]]>").await;
    assert_eq!(
        entity, cdata,
        "CDATA and entity-encoded HTML must extract identically"
    );
    assert!(entity.contains("Hello"));
}

// ---------------------------------------------------------------------------
// Atom xhtml content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn atom_xhtml_content_extracts() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><content type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>Hello <b>xhtml</b> world</p></div></content></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    // No <link> in this entry -> straight to embedded content, exercising
    // the xhtml routing path (feed-rs maps declared type="xhtml" content to
    // content_type text/html — verified against source; see route_text's
    // doc comment).
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Hello") && text.contains("xhtml") && text.contains("world"));
}

// ---------------------------------------------------------------------------
// `<content src=...>` treated as absent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_with_src_is_treated_absent_no_second_fetch() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><link href="https://feed.example.com/e1" rel="alternate"/><content type="text/html" src="https://feed.example.com/e1-media"/><summary>Fallback summary</summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::Gone, // force embedded fallback so routing reaches content/summary
    );
    let (ingestor, fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("Fallback summary"),
        "should fall through to summary: {text}"
    );
    assert_eq!(
        fetcher.call_count("https://feed.example.com/e1-media"),
        0,
        "the content src= URL must never be fetched"
    );
}

// ---------------------------------------------------------------------------
// Base64 binary content -> skipped piece
// ---------------------------------------------------------------------------

#[tokio::test]
async fn base64_octet_stream_content_falls_to_summary() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><content type="application/octet-stream">QUJDREVGRw==</content><summary>Fallback summary text</summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Fallback summary text"));
    assert!(
        !text.contains("QUJDREVGRw"),
        "base64 body must not appear verbatim"
    );
}

// ---------------------------------------------------------------------------
// Discovery-mode enrichment fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_mode_fetches_entry_pages_and_enriches_fully() {
    let entries = r#"<entry><title>Entry Title</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><published>2026-01-04T00:00:00Z</published><author><name>Alice</name></author><author><name>Bob</name></author><link href="https://feed.example.com/e1" rel="alternate"/></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::text_with_etag("# Page\n\nBody.\n", "etag-abc"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    let res = &cb.resources[0];
    assert_eq!(res.external_id.as_deref(), Some("urn:e1"));
    assert_eq!(res.external_etag.as_deref(), Some("etag-abc"));
    assert_eq!(
        res.metadata.dublin_core().creator,
        vec!["Alice".to_string(), "Bob".to_string()]
    );
    assert_eq!(
        res.metadata.dublin_core().source.as_deref(),
        Some("https://feed.example.com/feed.xml")
    );
    assert_eq!(
        res.metadata.dublin_core().date.as_deref(),
        Some("2026-01-04T00:00:00Z")
    );
}

// ---------------------------------------------------------------------------
// Two entries, one link
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_entries_sharing_one_link_dedup_keeps_newest() {
    // "First" is the NEWER entry (05 Jan) and sorts first; "Second" is
    // older (04 Jan). Both resolve to the same entry-link URI, so the
    // dedup pass (Codex review finding F3) must keep only "First" — the
    // sorted-first, i.e. newest, survivor — and the shared link is fetched
    // exactly once, not once per duplicate.
    let items = r#"<item><title>First</title><link>https://feed.example.com/shared</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>d1</description></item><item><title>Second</title><link>https://feed.example.com/shared</link><guid>e2</guid><pubDate>Sun, 04 Jan 2026 00:00:00 GMT</pubDate><description>d2</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/shared".to_string(),
        ScriptedOutcome::text("# Shared Page\n\nBody.\n"),
    );
    let (ingestor, fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    // Dedup by resolved URI, first (newest)-wins: exactly one Resource,
    // carrying the newer entry's identity, and only one fetch of the
    // shared link.
    assert_eq!(
        cb.discovered,
        vec![1],
        "on_discovered must reflect the post-dedup count"
    );
    assert_eq!(result.resources_produced, 1);
    assert_eq!(cb.resources.len(), 1);
    assert_eq!(cb.resources[0].external_id.as_deref(), Some("e1"));
    assert_eq!(
        fetcher.call_count("https://feed.example.com/shared"),
        1,
        "the shared link must be fetched exactly once, not once per duplicate"
    );
}

/// The bug this fixes (Codex review finding F3): on the fallback path (no
/// fetched page — here the entry link is `Gone`), each duplicate used to
/// fall back to its OWN embedded content, and because entries are processed
/// newest-first, the older entry's body landed last and won. With dedup by
/// resolved URI applied before processing, only the newest entry survives,
/// so its body is what gets indexed.
#[tokio::test]
async fn two_entries_sharing_gone_link_dedup_keeps_newest_body() {
    let items = r#"<item><title>First</title><link>https://feed.example.com/shared-gone</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Newer body</description></item><item><title>Second</title><link>https://feed.example.com/shared-gone</link><guid>e2</guid><pubDate>Sun, 04 Jan 2026 00:00:00 GMT</pubDate><description>Older body</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/shared-gone".to_string(),
        ScriptedOutcome::Gone,
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(cb.resources.len(), 1);
    assert_eq!(cb.resources[0].external_id.as_deref(), Some("e1"));
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("Newer body"),
        "the newer entry's body must win: {text}"
    );
    assert!(
        !text.contains("Older body"),
        "the older duplicate's body must not appear: {text}"
    );
}

/// `max_entries` counts distinct resource URIs, not raw entry count: a feed
/// with `[A, dup-of-A, B]` (newest-first) and `max_entries = 2` must yield
/// Resources for A and B, not A twice — duplicates must not burn slots.
#[tokio::test]
async fn max_entries_counts_distinct_uris_not_raw_entries() {
    let items = r#"<item><title>A</title><link>https://feed.example.com/a</link><guid>a1</guid><pubDate>Wed, 07 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item><item><title>A dup</title><link>https://feed.example.com/a</link><guid>a2</guid><pubDate>Tue, 06 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item><item><title>B</title><link>https://feed.example.com/b</link><guid>b1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/a".to_string(),
        ScriptedOutcome::text("# A\n"),
    );
    script.insert(
        "https://feed.example.com/b".to_string(),
        ScriptedOutcome::text("# B\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", Some(2), true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.discovered, vec![2]);
    assert_eq!(result.resources_produced, 2);
    let ids: Vec<&str> = cb
        .resources
        .iter()
        .map(|r| r.external_id.as_deref().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["a1", "b1"],
        "the A-duplicate must not burn a second max_entries slot"
    );
}

// ---------------------------------------------------------------------------
// Link-less entries: synthetic fragment URI
// ---------------------------------------------------------------------------

#[tokio::test]
async fn link_less_entry_gets_synthetic_fragment_uri() {
    let items = r#"<item><title>No Link Entry</title><guid>abc123</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    let expected = Uri::parse("https://feed.example.com/feed.xml#entry:abc123").unwrap();
    assert_eq!(cb.resources[0].uri, expected);
    // Round-trips through Uri::parse without losing the fragment (pinned by
    // core::uri::tests::parse_preserves_fragment_untouched).
    assert_eq!(
        cb.resources[0].uri.as_url().fragment(),
        Some("entry:abc123")
    );
    assert_eq!(
        fetcher.calls.lock().unwrap().len(),
        1,
        "only the feed itself was fetched, no entry page"
    );
}

/// Title-edit churn: a linked entry's URI is anchored to the link (stable
/// across title edits); a link-less entry's synthetic URI churns because
/// feed-rs's fallback id generator hashes in the title (see
/// `synthetic_entry_uri`'s doc comment). This test documents that asymmetry
/// rather than treating it as a bug.
#[tokio::test]
async fn title_edit_churn_linked_stable_link_less_churns() {
    async fn resource_uri_for(title: &str, with_link: bool, guid: Option<&str>) -> Uri {
        let link_xml = if with_link {
            "<link>https://feed.example.com/stable</link>"
        } else {
            ""
        };
        let guid_xml = guid
            .map(|g| format!("<guid>{g}</guid>"))
            .unwrap_or_default();
        let items = format!(
            r#"<item><title>{title}</title>{link_xml}{guid_xml}<pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body</description></item>"#
        );
        let feed_xml = rss2_feed("", &items);
        let mut script = HashMap::new();
        script.insert(
            "https://feed.example.com/feed.xml".to_string(),
            ScriptedOutcome::text(&feed_xml),
        );
        script.insert(
            "https://feed.example.com/stable".to_string(),
            ScriptedOutcome::text("# Page\n\nBody.\n"),
        );
        let (ingestor, _fetcher) = ingestor_with(script);
        let source = source_for("https://feed.example.com/feed.xml", None, true);
        let mut cb = RecordingCallback::default();
        ingestor.ingest(&source, &mut cb).await.unwrap();
        cb.resources[0].uri.clone()
    }

    // Linked entry: URI anchored to the link, no guid at all -> id is
    // link-derived and irrelevant to the URI either way.
    let linked_v1 = resource_uri_for("Original Title", true, None).await;
    let linked_v2 = resource_uri_for("Edited Title", true, None).await;
    assert_eq!(
        linked_v1, linked_v2,
        "a linked entry's URI must not churn on title edits"
    );

    // Link-less entry, no guid: feed-rs falls back to hashing
    // (base_uri, title) since there's no link — the synthetic fragment URI
    // churns when the title changes.
    let linkless_v1 = resource_uri_for("Original Title", false, None).await;
    let linkless_v2 = resource_uri_for("Edited Title", false, None).await;
    assert_ne!(
        linkless_v1, linkless_v2,
        "a link-less, guid-less entry's synthetic URI is expected to churn on title edits"
    );
}

// ---------------------------------------------------------------------------
// Entry with nothing at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entry_with_no_link_no_title_skips_error_siblings_still_processed() {
    let items = r#"<item><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate></item><item><title>Sibling</title><link>https://feed.example.com/sib</link><guid>sib</guid><pubDate>Sun, 04 Jan 2026 00:00:00 GMT</pubDate><description>Body</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/sib".to_string(),
        ScriptedOutcome::text("# Sibling Page\n\nBody.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.errors, 1);
    assert_eq!(result.resources_produced, 1);
    assert_eq!(cb.resources[0].external_id.as_deref(), Some("sib"));
    assert_eq!(
        cb.skipped
            .iter()
            .filter(|(_, r)| matches!(r, SkipReason::Error(_)))
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// Sort-before-take
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oldest_first_archive_feed_max_entries_picks_two_newest() {
    // Document (oldest-first) order in the XML on purpose.
    let items = r#"
        <item><title>Oldest</title><link>https://feed.example.com/1</link><guid>1</guid><pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate><description>d</description></item>
        <item><title>Middle</title><link>https://feed.example.com/2</link><guid>2</guid><pubDate>Tue, 01 Jan 2025 00:00:00 GMT</pubDate><description>d</description></item>
        <item><title>Newest</title><link>https://feed.example.com/3</link><guid>3</guid><pubDate>Wed, 01 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item>
    "#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/2".to_string(),
        ScriptedOutcome::text("# Two\n"),
    );
    script.insert(
        "https://feed.example.com/3".to_string(),
        ScriptedOutcome::text("# Three\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", Some(2), true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.discovered, vec![2]);
    assert_eq!(result.resources_produced, 2);
    let ids: Vec<&str> = cb
        .resources
        .iter()
        .map(|r| r.external_id.as_deref().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["3", "2"],
        "must keep the two NEWEST entries, newest first"
    );
}

/// Dedup must happen AFTER the sort, not before: an oldest-first archive
/// feed with a duplicated URI among its entries must still keep the newest
/// member of that duplicate group, exactly like
/// `oldest_first_archive_feed_max_entries_picks_two_newest` but with a
/// duplicate thrown into the document-order mix.
#[tokio::test]
async fn dedup_after_sort_oldest_first_archive_feed_with_duplicate() {
    // Document (oldest-first) order in the XML on purpose. "1" and "1-dup"
    // resolve to the same link but "1-dup" is newer than "1".
    let items = r#"
        <item><title>Oldest</title><link>https://feed.example.com/1</link><guid>1</guid><pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate><description>d</description></item>
        <item><title>Middle</title><link>https://feed.example.com/2</link><guid>2</guid><pubDate>Tue, 01 Jan 2025 00:00:00 GMT</pubDate><description>d</description></item>
        <item><title>Oldest dup, newer pubDate</title><link>https://feed.example.com/1</link><guid>1-dup</guid><pubDate>Wed, 01 Jan 2025 06:00:00 GMT</pubDate><description>d</description></item>
        <item><title>Newest</title><link>https://feed.example.com/3</link><guid>3</guid><pubDate>Wed, 01 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item>
    "#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/1".to_string(),
        ScriptedOutcome::text("# One\n"),
    );
    script.insert(
        "https://feed.example.com/2".to_string(),
        ScriptedOutcome::text("# Two\n"),
    );
    script.insert(
        "https://feed.example.com/3".to_string(),
        ScriptedOutcome::text("# Three\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    // No max_entries: assert full dedup + sort behaviour without truncation
    // interacting.
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.discovered, vec![3], "4 entries dedup to 3 distinct URIs");
    assert_eq!(result.resources_produced, 3);
    let ids: Vec<&str> = cb
        .resources
        .iter()
        .map(|r| r.external_id.as_deref().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["3", "1-dup", "2"],
        "newest-first order preserved, and the newer member of the /1 \
         duplicate group (1-dup, sorting ahead of 2) survives, not the \
         oldest (1)"
    );
}

// ---------------------------------------------------------------------------
// Fallback matrix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entry_link_gone_falls_back_to_embedded_content() {
    let items = r#"<item><title>E1</title><link>https://feed.example.com/gone</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Embedded body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/gone".to_string(),
        ScriptedOutcome::Gone,
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Embedded body text"));
}

#[tokio::test]
async fn entry_link_unsupported_falls_back_to_embedded_content() {
    let items = r#"<item><title>E1</title><link>https://feed.example.com/unsupported</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Embedded body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/unsupported".to_string(),
        ScriptedOutcome::text("binary garbage"),
    );
    let fetcher = std::sync::Arc::new(ScriptedFetcher::new(script));
    struct ArcFetcher(std::sync::Arc<ScriptedFetcher>);
    #[async_trait::async_trait]
    impl UrlFetcher for ArcFetcher {
        async fn fetch(&self, url: &str, meta: &FetchMetadata) -> Result<FetchResult, Error> {
            self.0.fetch(url, meta).await
        }
    }
    // NoneParser declines everything -> Unsupported on the entry page.
    let ingestor = FeedIngestor::new(
        Box::new(NoneParser),
        Box::new(ArcFetcher(fetcher.clone())),
        Box::new(ArcFetcher(fetcher)),
    );
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Embedded body text"));
}

#[tokio::test]
async fn entry_link_fetch_error_reports_error_no_fallback() {
    let items = r#"<item><title>E1</title><link>https://feed.example.com/err</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Embedded body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    // No entry for "err" -> ScriptedFetcher returns FetchError.
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.resources_produced, 0,
        "transient fetch failure must NOT fall back to embedded content"
    );
    assert_eq!(result.errors, 1);
    assert_eq!(cb.resources.len(), 0);
    assert_eq!(cb.skipped.len(), 1);
    assert!(matches!(&cb.skipped[0].1, SkipReason::Error(_)));
}

#[tokio::test]
async fn entry_link_gone_with_nothing_embedded_reports_error() {
    let items = r#"<item><title></title><link>https://feed.example.com/gone</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/gone".to_string(),
        ScriptedOutcome::Gone,
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 0);
    assert_eq!(result.errors, 1);
    assert_eq!(cb.skipped.len(), 1);
    assert!(matches!(&cb.skipped[0].1, SkipReason::Error(_)));
}

// ---------------------------------------------------------------------------
// Single-document mode template exactness + hash stability
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_doc_template_exactness() {
    // Atom, not RSS: RSS `<author>` maps its whole child text (typically
    // "email (Name)") into `Person.email` and hardcodes `Person.name` to the
    // literal string "author" (verified against feed-rs
    // `parser/rss2/mod.rs::handle_contact`) — Atom `<author><name>` is what
    // actually populates a human-readable `Person.name`.
    let entries = r#"<entry><title>Entry A</title><id>urn:a</id><updated>2026-01-05T00:00:00Z</updated><published>2026-01-05T00:00:00Z</published><author><name>Jane Doe</name></author><link href="https://feed.example.com/a" rel="alternate"/><summary>Body A</summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>();
    // Structural assertions rather than one giant literal-string match,
    // since exact Markdown->block splitting is an implementation detail of
    // `markdown_to_blocks`; what's pinned here is the template's content
    // and ordering.
    assert_eq!(text[0], "Test Feed", "feed title heading first");
    assert!(text.iter().any(|t| t == "Entry A"));
    // The byline and body land in the same coarse `Text` block (paragraph
    // breaks are not "structural boundaries" per `markdown_to_blocks`'s
    // block-splitting rules — only headings are), so assert containment
    // rather than an exact per-block match.
    assert!(text
        .iter()
        .any(|t| t.contains("Jane Doe") && t.contains("https://feed.example.com/a")));
    assert!(text.iter().any(|t| t.contains("Body A")));
}

#[tokio::test]
async fn single_doc_reordered_entries_and_bumped_feed_updated_same_hash() {
    let items_a = r#"<item><title>A</title><link>https://feed.example.com/a</link><guid>a</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body A</description></item><item><title>B</title><link>https://feed.example.com/b</link><guid>b</guid><pubDate>Sun, 04 Jan 2026 00:00:00 GMT</pubDate><description>Body B</description></item>"#;
    // Same entries, reversed document order.
    let items_b = r#"<item><title>B</title><link>https://feed.example.com/b</link><guid>b</guid><pubDate>Sun, 04 Jan 2026 00:00:00 GMT</pubDate><description>Body B</description></item><item><title>A</title><link>https://feed.example.com/a</link><guid>a</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body A</description></item>"#;

    async fn hash_for(items: &str) -> String {
        let feed_xml = rss2_feed("", items);
        let mut script = HashMap::new();
        script.insert(
            "https://feed.example.com/feed.xml".to_string(),
            ScriptedOutcome::text(&feed_xml),
        );
        let (ingestor, _fetcher) = ingestor_with(script);
        let source = source_for("https://feed.example.com/feed.xml", None, false);
        let mut cb = RecordingCallback::default();
        ingestor.ingest(&source, &mut cb).await.unwrap();
        cb.resources[0].content_hash.clone()
    }

    let hash_a = hash_for(items_a).await;
    let hash_b = hash_for(items_b).await;
    assert_eq!(
        hash_a, hash_b,
        "reordered entries must sort identically and hash identically"
    );
}

/// Single-document mode renders from the same `entries` collection that
/// discovery mode dedups before processing, so a duplicated entry must not
/// appear twice in the rendered feed document either.
#[tokio::test]
async fn single_doc_dedup_entry_listed_once() {
    let items = r#"<item><title>First</title><link>https://feed.example.com/shared</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Newer body text</description></item><item><title>Second</title><link>https://feed.example.com/shared</link><guid>e2</guid><pubDate>Sun, 04 Jan 2026 00:00:00 GMT</pubDate><description>Older body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        cb.resources.len(),
        1,
        "single-doc mode always emits one Resource"
    );
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>();
    let title_count = text
        .iter()
        .filter(|t| *t == "First" || *t == "Second")
        .count();
    assert_eq!(
        title_count, 1,
        "the duplicated entry must be listed once, not twice: {text:?}"
    );
    assert!(
        text.iter().any(|t| t == "First"),
        "the surviving (newer) entry's title must appear: {text:?}"
    );
    let joined = text.join(" ");
    assert!(joined.contains("Newer body text"));
    assert!(!joined.contains("Older body text"));
}

// ---------------------------------------------------------------------------
// Garbage pubDate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn garbage_pub_date_sorts_last_and_byline_omits_date() {
    let items = r#"<item><title>Bad Date</title><link>https://feed.example.com/bad</link><guid>bad</guid><pubDate>not a date at all</pubDate><description>d</description></item><item><title>Good Date</title><link>https://feed.example.com/good</link><guid>good</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>();
    let good_idx = text.iter().position(|t| t == "Good Date").unwrap();
    let bad_idx = text.iter().position(|t| t == "Bad Date").unwrap();
    assert!(
        good_idx < bad_idx,
        "the entry with a valid date must sort before the one with a garbage date"
    );

    // The garbage-date entry's byline must not contain a date token (no RFC
    // 3339 "T"+"Z"/offset pattern) — just author/link, if present, joined
    // with the em-dash separator, or nothing at all here (no author either).
    let bad_byline_present = text
        .iter()
        .any(|t| t.starts_with('*') && t.contains("feed.example.com/bad"));
    if bad_byline_present {
        let byline = text
            .iter()
            .find(|t| t.contains("feed.example.com/bad"))
            .unwrap();
        assert!(
            !byline.contains('T'),
            "byline for garbage-date entry must omit the date field: {byline}"
        );
    }
}

// ---------------------------------------------------------------------------
// Enclosures ignored
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enclosures_are_ignored() {
    let items = r#"<item><title>E1</title><link>https://feed.example.com/e1</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body</description><enclosure url="https://feed.example.com/audio.mp3" length="12345" type="audio/mpeg"/></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/e1".to_string(),
        ScriptedOutcome::text("# Page\n\nBody.\n"),
    );
    let (ingestor, fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(
        fetcher.call_count("https://feed.example.com/audio.mp3"),
        0,
        "the enclosure URL must never be fetched"
    );
}

// ---------------------------------------------------------------------------
// Redirect: resource uri = pre-redirect feed-declared link
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entry_link_redirect_resource_uri_is_pre_redirect_link() {
    let items = r#"<item><title>E1</title><link>https://feed.example.com/pre-redirect</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    // ScriptedFetcher is keyed on the pre-redirect URL — the "redirect" is
    // opaque to `UrlFetcher` (it just returns final bytes), so this is
    // exactly what a real fetcher following a redirect would do too.
    script.insert(
        "https://feed.example.com/pre-redirect".to_string(),
        ScriptedOutcome::text("# Post-redirect content\n\nBody.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(
        cb.resources[0].uri.as_str(),
        "https://feed.example.com/pre-redirect"
    );
}

// ---------------------------------------------------------------------------
// Redirect: relative entry links resolve against the effective (post-
// redirect) feed URL, not the configured one (Codex review finding F2).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relative_entry_link_resolves_against_effective_post_redirect_feed_url() {
    // A relative link: feed-rs resolves it against `base_uri` at parse time,
    // so `entry.links[0].href` (and therefore the Resource `uri`) is already
    // the resolved absolute URL by the time `FeedIngestor` sees it.
    let items = r#"<item><title>E1</title><link>article.html</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://old.example.com/feed.xml".to_string(),
        ScriptedOutcome::text_redirected_from(&feed_xml, "https://new.example.com/path/feed.xml"),
    );
    script.insert(
        "https://new.example.com/path/article.html".to_string(),
        ScriptedOutcome::text("# Post-redirect entry\n\nBody.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://old.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(
        cb.resources[0].uri.as_str(),
        "https://new.example.com/path/article.html",
        "a relative entry link must resolve against the effective (post-redirect) feed \
         URL and host, not the stale configured one"
    );
}

#[tokio::test]
async fn relative_entry_link_with_no_final_url_resolves_against_configured_feed_url() {
    // No redirect information available (`final_url: None`) -> falls back to
    // the configured URL as the resolution base, exactly like every other
    // existing fixture in this file (no behavior change when there's no
    // redirect).
    let items = r#"<item><title>E1</title><link>article.html</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>d</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://old.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://old.example.com/article.html".to_string(),
        ScriptedOutcome::text("# No-redirect entry\n\nBody.\n"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://old.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(
        cb.resources[0].uri.as_str(),
        "https://old.example.com/article.html",
        "with no final_url reported, resolution must fall back to the configured feed URL"
    );
}

#[tokio::test]
async fn redirected_feed_link_less_entry_fragment_uri_stays_pinned_to_configured_url() {
    // Identity vs. resolution split: a redirected feed's link-less entry
    // must still fragment off the CONFIGURED feed URL, never the effective
    // one — otherwise a transient redirect-target change would re-key every
    // link-less entry's Resource identity on every run.
    let items = r#"<item><title>No Link Entry</title><guid>nolink1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://old.example.com/feed.xml".to_string(),
        ScriptedOutcome::text_redirected_from(&feed_xml, "https://new.example.com/path/feed.xml"),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://old.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    let expected = Uri::parse("https://old.example.com/feed.xml#entry:nolink1").unwrap();
    assert_eq!(
        cb.resources[0].uri, expected,
        "a link-less entry's synthetic fragment URI must stay pinned to the CONFIGURED \
         feed URL, even when the feed fetch itself was redirected"
    );
    assert_eq!(
        cb.resources[0].metadata.dublin_core().source.as_deref(),
        Some("https://old.example.com/feed.xml"),
        "provenance_source must stay pinned to the configured feed URL too"
    );
}

#[tokio::test]
async fn redirected_feed_guidless_linkless_entry_id_stays_pinned_to_configured_url() {
    // The test above covers the fragment *prefix*; this covers the `{id}`
    // half, which has its own way of leaking the effective URL. An entry
    // with neither <guid> nor <link> has no id of its own, so feed-rs's
    // `assign_missing_ids` synthesizes one — and for a link-less entry
    // `generate_id` derives it from the parser's base URI plus the title.
    // Since the base URI is now the *effective* (post-redirect) feed URL,
    // that would make the whole synthetic Resource URI move the first time
    // a feed starts redirecting: the old URI gets delete-swept and the same
    // entry re-indexed under a new identity. Pinning the id generator's URI
    // argument to the configured `feed_url` keeps identity stable; link
    // resolution still uses the effective URL, because `generate_id` only
    // consults that argument on the link-less branch.
    let items = r#"<item><title>No Link No Guid</title><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body text</description></item>"#;
    let feed_xml = rss2_feed("", items);

    let mut direct = HashMap::new();
    direct.insert(
        "https://old.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (direct_ingestor, _direct_fetcher) = ingestor_with(direct);
    let source = source_for("https://old.example.com/feed.xml", None, true);
    let mut direct_cb = RecordingCallback::default();
    direct_ingestor
        .ingest(&source, &mut direct_cb)
        .await
        .unwrap();

    let mut redirected = HashMap::new();
    redirected.insert(
        "https://old.example.com/feed.xml".to_string(),
        ScriptedOutcome::text_redirected_from(&feed_xml, "https://new.example.com/path/feed.xml"),
    );
    let (redirected_ingestor, _redirected_fetcher) = ingestor_with(redirected);
    let mut redirected_cb = RecordingCallback::default();
    redirected_ingestor
        .ingest(&source, &mut redirected_cb)
        .await
        .unwrap();

    assert_eq!(direct_cb.resources.len(), 1);
    assert_eq!(redirected_cb.resources.len(), 1);
    assert_eq!(
        redirected_cb.resources[0].uri, direct_cb.resources[0].uri,
        "a guid-less, link-less entry must keep the same synthetic Resource URI \
         whether or not the feed fetch was redirected — otherwise the first \
         redirecting run re-keys it and delete-sweeps the previously indexed copy"
    );
}

// ---------------------------------------------------------------------------
// Non-http(s) entry links: never fetched, embedded content at the link URI
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mailto_link_entry_uses_embedded_content_at_link_uri_no_fetch() {
    let items = r#"<item><title>Mail Entry</title><link>mailto:someone@example.com</link><guid>m1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Mail body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    // A mailto: link parses as a valid Uri but is not fetchable — handing it
    // to the HTTP fetcher would be a transient FetchError every run (which
    // never falls back), so the entry's embedded content would never index.
    assert_eq!(result.errors, 0);
    assert_eq!(result.resources_produced, 1);
    // The parsed link stays the resource identity (stable, feed-declared).
    assert_eq!(cb.resources[0].uri.as_str(), "mailto:someone@example.com");
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Mail body text"));
    assert_eq!(
        fetcher.calls.lock().unwrap().len(),
        1,
        "only the feed itself is fetched, never the mailto link"
    );
}

// ---------------------------------------------------------------------------
// RSS <author>: feed-rs's literal "author" placeholder must not leak
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rss_author_placeholder_name_falls_back_to_email_in_creator() {
    let items = r#"<item><title>E1</title><guid>e1</guid><author>jane@example.com (Jane Doe)</author><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    assert_eq!(
        cb.resources[0].metadata.dublin_core().creator,
        vec!["jane@example.com (Jane Doe)".to_string()],
        "RSS <author> value lives in Person.email; the hardcoded \
         Person.name placeholder \"author\" must never appear as a creator"
    );
}

#[tokio::test]
async fn single_doc_rss_author_byline_uses_email_not_placeholder() {
    let items = r#"<item><title>E1</title><guid>e1</guid><author>jane@example.com (Jane Doe)</author><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Body</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("By jane@example.com (Jane Doe)"),
        "byline must carry the real author value: {text}"
    );
    assert!(
        !text.contains("By author"),
        "the literal \"author\" placeholder must not leak into the byline: {text}"
    );
}

// ---------------------------------------------------------------------------
// Empty extracted bodies fall through the content -> summary -> title chain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_extracted_content_falls_back_to_summary() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><content type="html">&lt;p&gt; &lt;/p&gt;</content><summary>Fallback summary text</summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    // A <content> that extracts to empty Markdown must not win the chain: a
    // 0-block Resource with a changed hash reaches core's empty-chunks arm,
    // which deletes the previously indexed document for this URI.
    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("Fallback summary text"),
        "empty content must fall through to the summary: {text}"
    );
    assert!(!cb.resources[0].blocks.is_empty());
}

#[tokio::test]
async fn empty_content_and_summary_fall_back_to_title_only() {
    let entries = r#"<entry><title>Only A Title</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><content type="html">&lt;div&gt;&lt;/div&gt;</content><summary>   </summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    assert!(
        !cb.resources[0].blocks.is_empty(),
        "an emitted resource must always have non-empty blocks"
    );
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Only A Title"));
}

// ---------------------------------------------------------------------------
// Fetched-page extracts empty (Codex review finding F1): must fall back to
// embedded content exactly like Gone/Unsupported, never index a 0-block
// Resource that would delete previously indexed content.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entry_link_extracts_empty_falls_back_to_summary() {
    let items = r#"<item><title>E1</title><link>https://feed.example.com/empty</link><guid>e1</guid><author>jane@example.com</author><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Fallback summary text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/empty".to_string(),
        // A 200 response whose body extracts (via PlainParser) to an empty
        // string — the fetched-page analog of F1's zero-block Resource.
        ScriptedOutcome::text(""),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    assert_eq!(cb.resources.len(), 1);
    let res = &cb.resources[0];
    assert_eq!(
        res.uri.as_str(),
        "https://feed.example.com/empty",
        "the fallback Resource must still carry the entry-link URI, not a synthetic one"
    );
    assert!(
        !res.blocks.is_empty(),
        "regression guard: an entry whose linked page extracts empty must never \
         produce a 0-block Resource (that's exactly the data-loss bug this fixes)"
    );
    let text = res
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("Fallback summary text"),
        "must fall back to the entry's own summary: {text}"
    );

    // Full feed enrichment must still apply on the fallback Resource.
    assert_eq!(res.external_id.as_deref(), Some("e1"));
    assert_eq!(
        res.metadata.dublin_core().creator,
        vec!["jane@example.com".to_string()]
    );
    assert!(res.metadata.dublin_core().date.is_some());
    assert_eq!(
        res.metadata.dublin_core().source.as_deref(),
        Some("https://feed.example.com/feed.xml")
    );
}

#[tokio::test]
async fn entry_link_extracts_empty_with_no_summary_falls_back_to_title_only() {
    let items = r#"<item><title>Only A Title</title><link>https://feed.example.com/empty</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/empty".to_string(),
        ScriptedOutcome::text(""),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(result.errors, 0);
    assert_eq!(cb.resources.len(), 1);
    assert!(
        !cb.resources[0].blocks.is_empty(),
        "regression guard: title-only fallback must still produce non-empty blocks"
    );
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Only A Title"));
}

#[tokio::test]
async fn entry_link_extracts_empty_with_nothing_embedded_reports_error() {
    let items = r#"<item><title></title><link>https://feed.example.com/empty</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "https://feed.example.com/empty".to_string(),
        ScriptedOutcome::text(""),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.resources_produced, 0,
        "no usable content anywhere in the chain must yield zero resources"
    );
    assert_eq!(result.errors, 1);
    assert!(cb.resources.is_empty());
    assert_eq!(cb.skipped.len(), 1);
    assert!(matches!(&cb.skipped[0].1, SkipReason::Error(_)));
}

// ---------------------------------------------------------------------------
// modified_at from feed timestamps (added_at stays ingestion-time)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modified_at_prefers_updated_while_dc_date_prefers_published() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><published>2026-01-04T00:00:00Z</published><summary>Body</summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    let res = &cb.resources[0];
    // modified_at = updated.or(published) (modification semantics);
    // dc.date = published.or(updated) (creation/publication semantics).
    assert_eq!(res.modified_at.as_deref(), Some("2026-01-05T00:00:00Z"));
    assert_eq!(
        res.metadata.dublin_core().date.as_deref(),
        Some("2026-01-04T00:00:00Z")
    );
    assert_ne!(
        Some(res.added_at.as_str()),
        res.modified_at.as_deref(),
        "added_at records when our store saw the entry, not the feed's date"
    );
}

/// A feed entry with no `updated`/`published` of its own makes no
/// modification-time claim at all: `modified_at` is `None`, never a
/// same-as-`added_at` `now()` stand-in (#283). `added_at` is still stamped
/// with the ingestion clock regardless.
#[tokio::test]
async fn no_entry_dates_yields_modified_at_none() {
    let items =
        r#"<item><title>No Dates</title><guid>e1</guid><description>Body</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    let res = &cb.resources[0];
    assert_eq!(
        res.modified_at, None,
        "a dateless entry makes no modification-time claim"
    );
    assert!(
        !res.added_at.is_empty(),
        "added_at must still be stamped from the ingestion clock"
    );
}

#[tokio::test]
async fn single_doc_modified_at_from_feed_updated() {
    // atom_feed's fixture declares <updated>2026-01-01T00:00:00Z</updated>
    // at feed level; the entry is newer — feed.updated still wins.
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><summary>Body</summary></entry>"#;
    let feed_xml = atom_feed(entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    assert_eq!(
        cb.resources[0].modified_at.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
}

// ---------------------------------------------------------------------------
// Large feed smoke test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn large_generated_feed_smoke_test_no_pathological_slowness() {
    let mut items = String::new();
    for i in 0..2000 {
        items.push_str(&format!(
            r#"<item><title>Entry {i}</title><guid>g{i}</guid><pubDate>Mon, 05 Jan 2026 00:00:{sec:02} GMT</pubDate></item>"#,
            i = i,
            sec = i % 60
        ));
    }
    let feed_xml = rss2_feed("", &items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();

    let start = std::time::Instant::now();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(cb.discovered, vec![2000]);
    assert_eq!(
        result.resources_produced, 2000,
        "every link-less, title-only entry should yield a resource"
    );
    assert!(
        elapsed.as_secs() < 10,
        "2000-entry feed took pathologically long: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Destination guard (SSRF): a blocked entry link degrades to embedded content
// ---------------------------------------------------------------------------

/// The user-visible half of the entry-link SSRF guard. `FetchResult::Blocked`
/// joins `Gone`/`Unsupported`/`Empty` in the fallback set, so an entry whose
/// link points at a non-routable destination is still indexed — from its own
/// embedded summary — rather than erroring or (worse) silently disappearing.
#[tokio::test]
async fn entry_link_blocked_falls_back_to_embedded_content() {
    let items = r#"<item><title>E1</title><link>http://127.0.0.1:8080/internal</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate><description>Embedded body text</description></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "http://127.0.0.1:8080/internal".to_string(),
        ScriptedOutcome::Blocked,
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(
        result.errors, 0,
        "a blocked destination is a policy decision, not a failure"
    );
    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(text.contains("Embedded body text"), "got: {text}");
}

/// The URI must still be reported even when the blocked entry has nothing to
/// fall back to — silence would be read by the delete-sweep as "gone".
#[tokio::test]
async fn entry_link_blocked_with_no_embedded_content_reports_the_uri() {
    let items = r#"<item><link>http://169.254.169.254/latest/meta-data/</link><guid>e1</guid><pubDate>Mon, 05 Jan 2026 00:00:00 GMT</pubDate></item>"#;
    let feed_xml = rss2_feed("", items);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    script.insert(
        "http://169.254.169.254/latest/meta-data/".to_string(),
        ScriptedOutcome::Blocked,
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 0);
    assert_eq!(result.errors, 1);
    assert_eq!(
        cb.skipped.len(),
        1,
        "the URI must be reported so the delete-sweep does not read silence as deletion"
    );
}

// ---------------------------------------------------------------------------
// Atom feed-level <author> inheritance (RFC 4287 §4.2.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_entry_inherits_feed_level_author() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><summary>Body</summary></entry>"#;
    let feed_xml = atom_feed_with(r#"<author><name>Feed Author</name></author>"#, entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    assert_eq!(
        cb.resources[0].metadata.dublin_core().creator,
        vec!["Feed Author".to_string()],
        "an authorless entry must inherit the feed-level <author>"
    );
}

#[tokio::test]
async fn discovery_entry_own_author_wins_over_feed_level() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><author><name>Entry Author</name></author><summary>Body</summary></entry>"#;
    let feed_xml = atom_feed_with(r#"<author><name>Feed Author</name></author>"#, entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        cb.resources[0].metadata.dublin_core().creator,
        vec!["Entry Author".to_string()],
        "the entry's own author wins outright; the two lists are never merged"
    );
}

#[tokio::test]
async fn single_doc_byline_inherits_feed_level_author() {
    let entries = r#"<entry><title>E1</title><id>urn:e1</id><updated>2026-01-05T00:00:00Z</updated><summary>Body</summary></entry><entry><title>E2</title><id>urn:e2</id><updated>2026-01-04T00:00:00Z</updated><author><name>Entry Author</name></author><summary>Body2</summary></entry>"#;
    let feed_xml = atom_feed_with(r#"<author><name>Feed Author</name></author>"#, entries);
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    let text = cb.resources[0]
        .blocks
        .iter()
        .map(|b| b.text.clone())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("By Feed Author"),
        "the authorless entry's byline must inherit the feed-level author: {text}"
    );
    assert!(
        text.contains("By Entry Author"),
        "the entry that declares its own author keeps it: {text}"
    );
}

// ---------------------------------------------------------------------------
// Feed-document conditional GET (sources.feed_etag / feed_last_modified)
// ---------------------------------------------------------------------------
//
// The feed document itself never becomes a `Resource` in discovery mode, so
// these validators live on the source row rather than a `resources` row (see
// specs/02-domain-model.md's Feed connector, "Conditional GET and pruning").
// `IngestSource::document_validators` carries what's stored in; a run
// reports what it observed back via `IngestResult::document_validators` for
// `run_source_ingestion` to thread into `IngestionResult` and `job_exec` to
// persist.

#[tokio::test]
async fn feed_document_fetch_sends_stored_validators() {
    let feed_xml = rss2_feed("", "");
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, fetcher) = ingestor_with(script);
    let source = source_with_validators(
        "https://feed.example.com/feed.xml",
        true,
        FetchMetadata {
            etag: Some("\"stored-etag\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
    );
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    let sent = fetcher
        .last_received_for("https://feed.example.com/feed.xml")
        .expect("the feed document must have been fetched");
    assert_eq!(
        sent.etag.as_deref(),
        Some("\"stored-etag\""),
        "the stored ETag must be replayed on the feed document's own fetch"
    );
    assert_eq!(
        sent.last_modified.as_deref(),
        Some("Wed, 21 Oct 2015 07:28:00 GMT")
    );
}

#[tokio::test]
async fn feed_document_200_reports_response_validators() {
    let feed_xml = rss2_feed("", "");
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text_with_validators(
            &feed_xml,
            "\"fresh-etag\"",
            "Thu, 22 Oct 2015 07:28:00 GMT",
        ),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.document_validators,
        Some(FetchMetadata {
            etag: Some("\"fresh-etag\"".to_string()),
            last_modified: Some("Thu, 22 Oct 2015 07:28:00 GMT".to_string()),
        }),
        "a 200 must report the response's own validators for persistence"
    );
}

/// The asymmetry between a 200 and a 304, pinned on its riskier half. A 304
/// carries no body, so silence about a validator means "unchanged" and the
/// stored value is kept. A 200 is a complete fresh representation, so
/// silence means the origin is no longer offering that validator and the
/// stored value must be cleared — otherwise the next run replays a validator
/// this origin never issued, and an origin that answers `412`/`200` to a
/// stale `If-None-Match` would be sent one indefinitely.
///
/// Every other 200 test starts from nothing stored, so all of them would
/// still pass if the clearing were dropped and a 200 merely folded like a
/// 304 does.
#[tokio::test]
async fn feed_document_200_without_validators_clears_the_stored_ones() {
    let feed_xml = rss2_feed("", "");
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text(&feed_xml),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_with_validators(
        "https://feed.example.com/feed.xml",
        true,
        FetchMetadata {
            etag: Some("\"stale-etag\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
    );
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.document_validators,
        Some(FetchMetadata {
            etag: None,
            last_modified: None,
        }),
        "a headerless 200 must report an empty FetchMetadata — `Some` so the \
         stored validators are replaced, and empty so they are cleared rather \
         than folded"
    );
}

#[tokio::test]
async fn feed_document_bare_304_reports_nothing() {
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::NotModified {
            etag: None,
            last_modified: None,
        },
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_with_validators(
        "https://feed.example.com/feed.xml",
        true,
        FetchMetadata {
            etag: Some("\"stored-etag\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
    );
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.document_validators, None,
        "a bare 304 must report nothing, so the stored row is left untouched"
    );
}

#[tokio::test]
async fn feed_document_304_with_refreshed_etag_folds_over_stored_last_modified() {
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::NotModified {
            etag: Some("\"rotated-etag\"".to_string()),
            last_modified: None,
        },
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_with_validators(
        "https://feed.example.com/feed.xml",
        true,
        FetchMetadata {
            etag: Some("\"stored-etag\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
    );
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.document_validators,
        Some(FetchMetadata {
            etag: Some("\"rotated-etag\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        }),
        "a 304 that rotated only the ETag must fold in the stored Last-Modified \
         rather than reporting it cleared"
    );
}

#[tokio::test]
async fn feed_document_304_with_refreshed_last_modified_folds_over_stored_etag() {
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::NotModified {
            etag: None,
            last_modified: Some("Thu, 22 Oct 2015 07:28:00 GMT".to_string()),
        },
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_with_validators(
        "https://feed.example.com/feed.xml",
        true,
        FetchMetadata {
            etag: Some("\"stored-etag\"".to_string()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".to_string()),
        },
    );
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.document_validators,
        Some(FetchMetadata {
            etag: Some("\"stored-etag\"".to_string()),
            last_modified: Some("Thu, 22 Oct 2015 07:28:00 GMT".to_string()),
        }),
        "a 304 that rotated only Last-Modified must fold in the stored ETag \
         rather than reporting it cleared"
    );
}

#[tokio::test]
async fn single_document_mode_stamps_feed_validators_onto_its_resource() {
    let feed_xml = rss2_feed("", "");
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::text_with_validators(
            &feed_xml,
            "\"doc-etag\"",
            "Fri, 23 Oct 2015 07:28:00 GMT",
        ),
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    // Single-doc mode: `fetch_full_content: false`.
    let source = source_for("https://feed.example.com/feed.xml", None, false);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 1);
    assert_eq!(cb.resources.len(), 1);
    assert_eq!(
        cb.resources[0].external_etag.as_deref(),
        Some("\"doc-etag\""),
        "the feed document's own validators belong on its Resource in single-doc mode"
    );
    assert_eq!(
        cb.resources[0].external_last_modified.as_deref(),
        Some("Fri, 23 Oct 2015 07:28:00 GMT")
    );
    // Discovery-mode's `sources.feed_etag`/`feed_last_modified` reporting is
    // orthogonal to this — single-doc mode still reports via
    // `result.document_validators` too, since the fetch that produced this
    // Resource is the same fetch of the feed document.
    assert_eq!(
        result.document_validators,
        Some(FetchMetadata {
            etag: Some("\"doc-etag\"".to_string()),
            last_modified: Some("Fri, 23 Oct 2015 07:28:00 GMT".to_string()),
        })
    );
}

// ---------------------------------------------------------------------------
// Enumeration: which failures mean "this run never read the window"
// ---------------------------------------------------------------------------

/// Every early return that leaves the feed document unread must report
/// `Enumeration::Incomplete`, because `Complete` is the `IngestResult`
/// default and silence would otherwise read as "we saw the whole window and
/// it holds nothing".
///
/// The consequence is not the presumed-gone delete-sweep — feed sources are
/// exempt from that — but the feed liveness sweep, which under `--delete`
/// probes entry links and prunes on a confirmed 404/410. Its one inherited
/// guard keys on exactly this field. Without it, a run whose feed fetch or
/// parse failed still swept: the seen-set is empty on such a run, so nothing
/// excludes the entries the window still lists, and up to 25 of them get
/// probed and pruned off an origin already known to be misbehaving. A
/// successful run would have excluded every one of them.
#[tokio::test]
async fn a_run_that_never_read_the_window_reports_incomplete_enumeration() {
    // (label, scripted feed-document outcome or None for a fetch error)
    let cases: Vec<(&str, Option<ScriptedOutcome>)> = vec![
        ("fetch error", None),
        ("feed root 404/410", Some(ScriptedOutcome::Gone)),
        ("destination blocked", Some(ScriptedOutcome::Blocked)),
        (
            "unparseable body",
            Some(ScriptedOutcome::text("<rss><this is <<not valid xml")),
        ),
    ];

    for (label, outcome) in cases {
        let mut script = HashMap::new();
        if let Some(o) = outcome {
            script.insert("https://feed.example.com/feed.xml".to_string(), o);
        }
        let (ingestor, _fetcher) = ingestor_with(script);
        let source = source_for("https://feed.example.com/feed.xml", None, true);
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        match &result.enumeration {
            Enumeration::Incomplete { reason } => assert!(
                !reason.is_empty(),
                "{label}: the reason is interpolated into the suppression warning"
            ),
            Enumeration::Complete => panic!(
                "{label}: this run never read the feed's window, so it must not claim \
                 Complete — the liveness sweep would probe and prune off a broken signal"
            ),
        }
    }
}
