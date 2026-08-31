//! `tool_search` and `tool_get_chunks` tests: limit/content_length
//! resolution, empty-result and error shapes, the `store` discriminator
//! (#144), anchor-relative pagination argument resolution, and
//! `render_citations_text` rendering.

use rmcp::model::CallToolResult;

use localdb_core::embedder::FakeEmbedder;
use localdb_core::store::{FakeStore, RetrievalStore};
use localdb_core::SearchFilters;

use crate::args::{GetChunksArgs, SearchArgs};
use crate::tools::{
    render_citations_text, resolve_content_length, resolve_limit, resolve_offset,
    resolve_search_limit, tool_get_chunks, tool_search, typed_error, AvailableStore,
    SEARCH_DEFAULT_LIMIT,
};

use super::common::{duplicate_doc_stores, make_chunk, make_descriptor, text_of};

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn search_args(query: &str) -> SearchArgs {
    SearchArgs {
        query: query.to_string(),
        stores: None,
        limit: None,
        content_length: None,
        filters: SearchFilters::default(),
    }
}

fn get_chunks_args(resource_id: &str) -> GetChunksArgs {
    GetChunksArgs {
        resource_id: resource_id.to_string(),
        offset: None,
        limit: None,
        anchor_chunk_id: None,
        anchor_block_seq: None,
        store: None,
    }
}

/// Resolve `GetChunksArgs::offset`/`limit` to validated `usize`s.
///
/// A thin wrapper over [`resolve_offset`]/[`resolve_limit`], kept only for
/// their dedicated unit tests below (`tool_get_chunks` itself calls the two
/// underlying functions separately, since the anchor path needs `limit`
/// resolved before `offset` even applies).
#[allow(clippy::result_large_err)] // see note on select_mcp_stores in tools/mod.rs
fn resolve_get_chunks_pagination(args: &GetChunksArgs) -> Result<(usize, usize), CallToolResult> {
    let offset = resolve_offset(args.offset)?;
    let limit = resolve_limit(args.limit)?;
    Ok((offset, limit))
}

// -----------------------------------------------------------------------
// E4 — search rejects limit=0
// -----------------------------------------------------------------------

#[tokio::test]
async fn search_tool_rejects_limit_zero() {
    let store = FakeStore::new();
    let av = AvailableStore::new(make_descriptor("store-1", "mystore"), Box::new(store));
    let embedder = FakeEmbedder::new(128);
    let mut args = search_args("hello");
    args.limit = Some(0);
    let result = tool_search(&[av], &embedder, args).await;
    assert_eq!(
        result.is_error,
        Some(true),
        "limit=0 should produce an error result"
    );
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("error body is JSON");
    assert_eq!(
        parsed["error"]["code"].as_str().unwrap(),
        "invalid_request",
        "error code should be invalid_request"
    );
    assert!(
        parsed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("limit must be at least 1"),
        "error message should mention limit"
    );
}

#[test]
fn resolve_search_limit_zero_passes_through() {
    // resolve_search_limit does not reject limit=0 itself (that's the
    // tool's job) — 0 must survive unchanged so the tool-level guard fires.
    assert_eq!(resolve_search_limit(Some(0)), 0);
}

#[test]
fn resolve_search_limit_negative_falls_back_to_default() {
    // Mirrors the old raw-JSON `Value::as_u64()` parse, which failed on
    // negative numbers and silently defaulted.
    assert_eq!(resolve_search_limit(Some(-5)), SEARCH_DEFAULT_LIMIT);
}

// -----------------------------------------------------------------------
// Metadata filters — per-field parsing is unit-tested once in
// `localdb_core::search_filters::tests`; these two prove the wiring: a
// filter narrows results, and a malformed value is a tool-level
// `invalid_request` error, not a panic or a silently-empty result.
// -----------------------------------------------------------------------

#[tokio::test]
async fn search_path_filter_narrows_results() {
    let store = FakeStore::new();
    let keep = make_chunk(
        "chunk-keep",
        "keep-doc",
        "store-1",
        "hello world rust programming",
    );
    let skip = make_chunk(
        "chunk-skip",
        "skip-doc",
        "store-1",
        "hello world rust programming",
    );
    store.upsert_chunks(vec![keep, skip]).await.unwrap();
    let av = AvailableStore::new(make_descriptor("store-1", "mystore"), Box::new(store));
    let embedder = FakeEmbedder::new(128);

    let mut args = search_args("hello world rust programming");
    args.filters.path = Some("file:///docs/keep".to_string());
    let result = tool_search(&[av], &embedder, args).await;
    assert_ne!(result.is_error, Some(true), "unexpected error: {result:?}");

    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(
        text.split("\n\n---\n")
            .next()
            .expect("text rendering has a JSON prefix"),
    )
    .expect("JSON prefix parses");
    let citations = parsed["citations"].as_array().expect("citations array");
    assert!(
        !citations.is_empty(),
        "expected at least one match: {parsed:?}"
    );
    for citation in citations {
        let uri = citation["uri"].as_str().unwrap();
        assert!(
            uri.starts_with("file:///docs/keep"),
            "path filter should only return matching citations, got uri {uri}"
        );
    }
}

#[tokio::test]
async fn search_bad_filter_value_returns_invalid_request() {
    let store = FakeStore::new();
    let av = AvailableStore::new(make_descriptor("store-1", "mystore"), Box::new(store));
    let embedder = FakeEmbedder::new(128);

    let mut args = search_args("hello");
    args.filters.added_after = Some("not-a-date".to_string());
    let result = tool_search(&[av], &embedder, args).await;
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("error body is JSON");
    assert_eq!(
        parsed["error"]["code"].as_str().unwrap(),
        "invalid_request",
        "error code should be invalid_request"
    );
}

/// The same malformed value must be rejected when the session exposes no
/// stores at all. Argument validity does not depend on store availability:
/// behind the empty-store early return, this was reported as a successful
/// empty search, so an agent got `{"citations": []}` for a request that was
/// never valid.
#[tokio::test]
async fn search_bad_filter_value_returns_invalid_request_with_no_stores() {
    let embedder = FakeEmbedder::new(128);

    let mut args = search_args("hello");
    args.filters.added_after = Some("not-a-date".to_string());
    let result = tool_search(&[], &embedder, args).await;

    assert_eq!(
        result.is_error,
        Some(true),
        "a malformed filter must not be reported as a successful empty search"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&text_of(&result)).expect("error body is JSON");
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}

// -----------------------------------------------------------------------
// E2 — typed error shape
// -----------------------------------------------------------------------

#[test]
fn typed_error_helper_produces_correct_shape() {
    let result = typed_error("store_not_found", "no store named 'foo'");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
    assert!(parsed["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no store named 'foo'"));
}

#[tokio::test]
async fn search_returns_empty_citations_not_error_when_no_results() {
    // E2 also requires: 0 results → {"citations": []} not an error.
    let fake = FakeStore::new();
    let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));
    let embedder = FakeEmbedder::new(128);

    let args = search_args("totally absent term xyzzy");
    let result = tool_search(&[av], &embedder, args).await;
    // Should NOT be an error — just empty citations.
    assert_ne!(
        result.is_error,
        Some(true),
        "empty results should not be an error"
    );
}

#[tokio::test]
async fn search_unknown_store_returns_typed_error() {
    let fake = FakeStore::new();
    let av = AvailableStore::new(make_descriptor("store-1", "real-store"), Box::new(fake));
    let embedder = FakeEmbedder::new(128);

    let mut args = search_args("hello");
    args.stores = Some(vec!["nonexistent-store".to_string()]);
    let result = tool_search(&[av], &embedder, args).await;
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
}

// -----------------------------------------------------------------------
// #144 — `store` discriminator on get_document / get_chunks
// -----------------------------------------------------------------------

#[tokio::test]
async fn get_chunks_with_store_name_disambiguates_duplicate_id_across_stores() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];

    let mut args_a = get_chunks_args("dup-doc");
    args_a.store = Some("store-a".to_string());
    let result_a = tool_get_chunks(&stores, args_a).await;
    assert_ne!(result_a.is_error, Some(true));
    let parsed_a: serde_json::Value = serde_json::from_str(&text_of(&result_a)).unwrap();
    assert_eq!(
        parsed_a["chunks"][0]["text"].as_str().unwrap(),
        "from store A"
    );
    assert_eq!(parsed_a["store"]["name"].as_str().unwrap(), "store-a");

    let mut args_b = get_chunks_args("dup-doc");
    args_b.store = Some("store-b".to_string());
    let result_b = tool_get_chunks(&stores, args_b).await;
    assert_ne!(result_b.is_error, Some(true));
    let parsed_b: serde_json::Value = serde_json::from_str(&text_of(&result_b)).unwrap();
    assert_eq!(
        parsed_b["chunks"][0]["text"].as_str().unwrap(),
        "from store B"
    );
    assert_eq!(parsed_b["store"]["name"].as_str().unwrap(), "store-b");
}

#[tokio::test]
async fn get_chunks_with_store_id_also_disambiguates() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];

    let mut args = get_chunks_args("dup-doc");
    args.store = Some("store-A-id".to_string());
    let result = tool_get_chunks(&stores, args).await;
    assert_ne!(result.is_error, Some(true));
    let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(
        parsed["chunks"][0]["text"].as_str().unwrap(),
        "from store A"
    );
    assert_eq!(parsed["store"]["id"].as_str().unwrap(), "store-A-id");
}

#[tokio::test]
async fn get_chunks_unknown_store_returns_store_not_found() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];

    let mut args = get_chunks_args("dup-doc");
    args.store = Some("no-such-store".to_string());
    let result = tool_get_chunks(&stores, args).await;
    assert_eq!(result.is_error, Some(true));
    let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "store_not_found");
}

#[tokio::test]
async fn get_chunks_omitted_store_keeps_first_match_backward_compat() {
    let (av_a, av_b) = duplicate_doc_stores("dup-doc").await;
    let stores = vec![av_a, av_b];

    let args = get_chunks_args("dup-doc");
    let result = tool_get_chunks(&stores, args).await;
    assert_ne!(result.is_error, Some(true));
    let parsed: serde_json::Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(
        parsed["chunks"][0]["text"].as_str().unwrap(),
        "from store A",
        "omitted store must keep pre-#144 first-match-wins behavior"
    );
}

// -----------------------------------------------------------------------
// render_citations_text — creator · date formatting
// -----------------------------------------------------------------------

fn make_citation_with_metadata(
    uri: &str,
    creator: Vec<String>,
    date: Option<String>,
) -> localdb_core::citation::Citation {
    use localdb_core::{
        citation::{
            ChunkPosition, CitationBlock, CitationLocation, CitationProvenance, CitationStore,
            Score,
        },
        metadata::{DocumentMetadata, DublinCoreMetadata, Metadata},
        types::Span,
    };
    localdb_core::citation::Citation {
        chunk_id: "c1".to_string(),
        resource_id: "d1".to_string(),
        store: CitationStore {
            id: "s1".to_string(),
            name: "store".to_string(),
        },
        uri: uri.to_string(),
        title: None,
        heading_path: vec![],
        block: CitationBlock {
            seq: 0,
            kind: None,
            page: None,
        },
        chunk_position: ChunkPosition { seq_in_block: 0 },
        location: CitationLocation {
            span: Span::new(0, 4),
            window_block_seqs: vec![],
        },
        snippet: "text".to_string(),
        score: Score {
            fused: 0.5,
            dense: None,
            bm25: None,
        },
        provenance: CitationProvenance {
            fetched_at: "2026-01-01T00:00:00Z".to_string(),
            content_hash: "abc".to_string(),
        },
        metadata: Metadata::Document(DocumentMetadata {
            dublin_core: DublinCoreMetadata {
                creator,
                date,
                ..Default::default()
            },
            ..Default::default()
        }),
    }
}

#[test]
fn render_citations_text_shows_creator_and_date() {
    let c = make_citation_with_metadata(
        "file:///a.md",
        vec!["Alice".to_string()],
        Some("2026-03-01".to_string()),
    );
    let text = render_citations_text(&[c], 400);
    assert!(
        text.contains("Alice · 2026-03-01"),
        "should show creator · date, got: {text}"
    );
}

#[test]
fn render_citations_text_date_only() {
    let c = make_citation_with_metadata("file:///a.md", vec![], Some("2026-03-01".to_string()));
    let text = render_citations_text(&[c], 400);
    assert!(text.contains("2026-03-01"), "should show date, got: {text}");
    assert!(!text.contains('·'), "should not show · with no creator");
}

#[test]
fn render_citations_text_creator_only() {
    let c = make_citation_with_metadata("file:///a.md", vec!["Bob".to_string()], None);
    let text = render_citations_text(&[c], 400);
    assert!(text.contains("Bob"), "should show creator, got: {text}");
    assert!(!text.contains('·'), "should not show · with no date");
}

#[test]
fn render_citations_text_no_metadata() {
    let c = make_citation_with_metadata("file:///a.md", vec![], None);
    let text = render_citations_text(&[c], 400);
    assert!(!text.contains('·'), "no metadata — no · separator");
}

#[test]
fn render_citations_text_respects_custom_content_length() {
    let mut c = make_citation_with_metadata("file:///a.md", vec![], None);
    c.snippet = "word ".repeat(200);
    let text = render_citations_text(&[c], 50);
    let snippet_line = text
        .lines()
        .find(|l| l.trim_start().starts_with("word"))
        .unwrap();
    assert!(
        snippet_line.trim().chars().count() <= 50,
        "snippet should be capped at 50 chars, got: {snippet_line}"
    );
}

#[test]
fn render_citations_text_snaps_to_sentence_boundary() {
    let mut c = make_citation_with_metadata("file:///a.md", vec![], None);
    c.snippet =
        "This is sentence one. This is sentence two that keeps going and going and going further."
            .to_string();
    let text = render_citations_text(&[c], 25);
    let snippet_line = text
        .lines()
        .find(|l| l.trim_start().starts_with("This"))
        .unwrap()
        .trim();
    assert!(
        snippet_line.ends_with('…'),
        "expected ellipsis marker, got: {snippet_line}"
    );
    // The char immediately before the ellipsis must be the sentence
    // terminator, not a mid-word letter.
    let before_ellipsis = snippet_line
        .chars()
        .rev()
        .nth(1)
        .expect("snippet should have content before the ellipsis");
    assert_eq!(
        before_ellipsis, '.',
        "expected sentence-boundary cut, got: {snippet_line}"
    );
}

#[test]
fn search_args_default_content_length() {
    assert_eq!(
        resolve_content_length(None),
        400,
        "default content_length should be 400"
    );
}

#[test]
fn search_args_custom_content_length() {
    assert_eq!(resolve_content_length(Some(50)), 50);
}

// -----------------------------------------------------------------------
// GetChunksArgs pagination resolution
// -----------------------------------------------------------------------

#[test]
fn get_chunks_args_limit_clamped_to_max() {
    let mut args = get_chunks_args("doc-1");
    args.limit = Some(9999);
    let (_, limit) = resolve_get_chunks_pagination(&args).expect("should parse");
    assert_eq!(limit, 200, "limit should be clamped to MAX_LIMIT=200");
}

#[test]
fn get_chunks_args_zero_limit_is_invalid_request() {
    // The schema requires limit >= 1; an explicit 0 must be rejected rather
    // than clamped up to 1 (which would return a chunk the caller did not
    // ask for).
    let mut args = get_chunks_args("doc-1");
    args.limit = Some(0);
    assert_invalid_request(resolve_get_chunks_pagination(&args));
}

#[test]
fn get_chunks_args_defaults() {
    let args = get_chunks_args("doc-1");
    let (offset, limit) = resolve_get_chunks_pagination(&args).expect("should parse");
    assert_eq!(offset, 0, "default offset should be 0");
    assert_eq!(limit, 50, "default limit should be 50");
}

#[tokio::test]
async fn get_chunks_empty_resource_id_is_invalid_request() {
    let fake = FakeStore::new();
    let av = AvailableStore::new(make_descriptor("store-1", "s1"), Box::new(fake));
    let args = get_chunks_args("   ");
    let result = tool_get_chunks(&[av], args).await;
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}

/// Assert a `resolve_get_chunks_pagination` failure carries the `invalid_request` code.
fn assert_invalid_request(result: Result<(usize, usize), CallToolResult>) {
    let err = result.expect_err("expected an error result");
    assert_eq!(err.is_error, Some(true));
    let text = err.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "invalid_request");
}

#[test]
fn get_chunks_args_negative_offset_is_invalid_request() {
    // A present-but-negative offset must be rejected, not silently defaulted to 0.
    let mut args = get_chunks_args("doc-1");
    args.offset = Some(-1);
    assert_invalid_request(resolve_get_chunks_pagination(&args));
}

#[test]
fn get_chunks_args_negative_limit_is_invalid_request() {
    // A present-but-negative limit must be rejected, not silently defaulted.
    let mut args = get_chunks_args("doc-1");
    args.limit = Some(-5);
    assert_invalid_request(resolve_get_chunks_pagination(&args));
}

#[test]
fn render_citations_empty() {
    let text = render_citations_text(&[], 400);
    assert_eq!(text, "No results found.");
}
