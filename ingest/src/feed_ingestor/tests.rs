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
    },
    NotModified,
    Gone,
}

impl ScriptedOutcome {
    fn text(body: &str) -> Self {
        ScriptedOutcome::Downloaded {
            bytes: body.as_bytes().to_vec(),
            content_type: None,
            etag: None,
        }
    }

    fn text_with_etag(body: &str, etag: &str) -> Self {
        ScriptedOutcome::Downloaded {
            bytes: body.as_bytes().to_vec(),
            content_type: None,
            etag: Some(etag.to_string()),
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
}

impl ScriptedFetcher {
    fn new(script: HashMap<String, ScriptedOutcome>) -> Self {
        Self {
            script,
            calls: Mutex::new(Vec::new()),
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
}

#[async_trait::async_trait]
impl UrlFetcher for ScriptedFetcher {
    async fn fetch(&self, url: &str, _meta: &FetchMetadata) -> Result<FetchResult, Error> {
        self.calls.lock().unwrap().push(url.to_string());
        match self.script.get(url) {
            Some(ScriptedOutcome::Downloaded {
                bytes,
                content_type,
                etag,
            }) => Ok(FetchResult::Downloaded {
                bytes: bytes.clone(),
                content_type: content_type.clone(),
                etag: etag.clone(),
                last_modified: None,
            }),
            Some(ScriptedOutcome::NotModified) => Ok(FetchResult::NotModified),
            Some(ScriptedOutcome::Gone) => Ok(FetchResult::Gone),
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
    let ingestor = FeedIngestor::new(Box::new(PlainParser), Box::new(ArcFetcher(fetcher.clone())));
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
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom"><title>Test Feed</title><id>urn:test-feed</id><updated>2026-01-01T00:00:00Z</updated><link href="https://feed.example.com/" rel="alternate"/>{entries_xml}</feed>"#
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
    };
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Feed-level fetch outcomes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn feed_level_not_modified_is_single_unchanged_skip_no_entry_callbacks() {
    let mut script = HashMap::new();
    script.insert(
        "https://feed.example.com/feed.xml".to_string(),
        ScriptedOutcome::NotModified,
    );
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.resources_produced, 0);
    assert_eq!(result.errors, 0);
    assert!(
        cb.discovered.is_empty(),
        "no on_discovered on feed-level 304"
    );
    assert!(cb.resources.is_empty());
    assert_eq!(cb.skipped.len(), 1);
    assert_eq!(cb.skipped[0].1, SkipReason::Unchanged);
}

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
        Some("2026-01-04T00:00:00+00:00")
    );
}

// ---------------------------------------------------------------------------
// Two entries, one link
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_entries_sharing_one_link_both_processed() {
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
    let (ingestor, _fetcher) = ingestor_with(script);
    let source = source_for("https://feed.example.com/feed.xml", None, true);
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    // No pre-dedup: both entries individually run process_url against the
    // same locator/URI. The pipeline's own content-hash skip (not this
    // ingestor) is what would absorb the second one in a real store; from
    // the ingestor's point of view both are reported.
    assert_eq!(result.resources_produced, 2);
    assert_eq!(cb.resources.len(), 2);
    assert_eq!(cb.resources[0].uri, cb.resources[1].uri);
    assert_eq!(cb.resources[0].external_id.as_deref(), Some("e1"));
    assert_eq!(cb.resources[1].external_id.as_deref(), Some("e2"));
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
    let ingestor = FeedIngestor::new(Box::new(NoneParser), Box::new(ArcFetcher(fetcher)));
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
    assert_eq!(res.modified_at, "2026-01-05T00:00:00+00:00");
    assert_eq!(
        res.metadata.dublin_core().date.as_deref(),
        Some("2026-01-04T00:00:00+00:00")
    );
    assert_ne!(
        res.added_at, res.modified_at,
        "added_at records when our store saw the entry, not the feed's date"
    );
}

#[tokio::test]
async fn no_entry_dates_added_at_equals_modified_at() {
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
    assert_eq!(res.added_at, res.modified_at);
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
    assert_eq!(cb.resources[0].modified_at, "2026-01-01T00:00:00+00:00");
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
