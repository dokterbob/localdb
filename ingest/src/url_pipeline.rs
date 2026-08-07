//! Shared per-URL fetch/parse/enrich pipeline used by [`crate::url_ingestor`]
//! and [`crate::feed_ingestor`] (issue #116).
//!
//! `UrlIngestor`'s per-URL loop body (fetch → sniff → parse → title-merge →
//! `Resource` construction) is identical work that a feed ingestor needs to
//! do once per entry, plus a handful of enrichment fields (external_id,
//! author, date, provenance source) that a plain URL fetch never has. This
//! module extracts that body behind [`process_url`], parameterized by a
//! [`ResourceEnrichment`] whose `Default` reproduces `UrlIngestor`'s exact
//! prior behavior byte-for-byte (see `url_ingestor::tests` for the pinning
//! test).
//!
//! Reporting split (pinned by design review, do not re-litigate):
//! - `NotModified` -> `on_skipped(Unchanged)` + `resources_skipped += 1`.
//! - `FetchError` / `ParseFailed` (incl. parser panic and parser `Err`) ->
//!   `on_skipped(Error)` + `errors += 1`.
//! - `Gone` stays silent — no callback at all (delete-sweep contract).
//! - `Unsupported` returns WITHOUT reporting anything — the caller decides:
//!   `UrlIngestor` reports `on_skipped(Unsupported) + resources_skipped += 1`
//!   immediately (preserving its existing behavior); `FeedIngestor` instead
//!   attempts an embedded-content fallback first and only reports if that
//!   also fails.
//! - `Empty` (the parser accepted the format and returned `Ok(Some(doc))`,
//!   but `doc.markdown` trims to nothing) ALSO returns WITHOUT reporting
//!   anything — the caller decides, exactly like `Unsupported`. Kept as a
//!   distinct variant rather than folded into `Unsupported` because the two
//!   mean different things: `Unsupported` is "no parser handles this
//!   format", `Empty` is "a parser accepted it and produced nothing". A
//!   fetched page that extracts to zero content must never reach
//!   `on_resource`: a 0-block `Resource` with a changed content hash hits
//!   `index_resource`'s empty-chunks arm, which deletes any previously
//!   indexed content for the URI and reports it as indexed (Codex review
//!   finding F1) — silent data loss. `UrlIngestor` reports
//!   `on_skipped(Other("...")) + resources_skipped += 1` immediately (NOT
//!   `Unsupported` — see `url_ingestor`'s match arm for why); `FeedIngestor`
//!   folds it into the same fallback chain as `Gone`/`Unsupported`.
//! - `Indexed` calls `on_resource` and bumps `resources_produced` internally.

use extract::sniff_mime;
use localdb_core::block::{IngestorKind, Resource, ResourceKind};
use localdb_core::error::Error;
use localdb_core::ids::resource_id;
use localdb_core::ingestion::{now_rfc3339, FetchMetadata, FetchResult, UrlFetcher};
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, SkipReason};
use localdb_core::markdown_blocks::{compute_blocks_hash, markdown_to_blocks};
use localdb_core::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};
use localdb_core::parser::{Parser, Probe};
use localdb_core::uri::Uri;

use crate::support::catch_panic;

/// What happened to a single locator after [`process_url`] ran.
///
/// Callers branch on this to decide whether any additional handling (a
/// caller-side report, or — for `FeedIngestor` — an embedded-content
/// fallback) is needed. See the module doc for the full reporting contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlOutcome {
    Indexed,
    Unchanged,
    Gone,
    /// The fetcher refused the destination (see `FetchResult::Blocked`).
    /// Reported like `Gone` from here — log and return, no callback — so the
    /// caller decides. Unlike `Gone`, though, "no callback" is NOT a
    /// delete-sweep signal the caller may rely on: `UrlIngestor` must report
    /// this explicitly (see its match arm), and `FeedIngestor` folds it into
    /// the embedded-content fallback chain.
    Blocked,
    Unsupported,
    /// The parser accepted the format (`Ok(Some(doc))`) but `doc.markdown`
    /// trims to nothing. Distinct from `Unsupported` — see the module doc.
    Empty,
    FetchError,
    ParseFailed,
}

/// Extra metadata a caller can thread into the `Resource` built by
/// [`process_url`], beyond what a bare URL fetch/parse produces.
///
/// `Default` reproduces `UrlIngestor`'s pre-refactor behavior exactly: no
/// external id, no title fallback (the page's own title or Dublin Core title
/// wins or the field stays `None`), no injected creator/date/provenance,
/// `added_at == modified_at` (no `modified_at_override`), and
/// `external_etag` always `None` (a bare `UrlIngestor` never threads
/// conditional-fetch state through `Resource`).
#[derive(Default)]
pub(crate) struct ResourceEnrichment {
    /// Arbitrary source-system ID (e.g. a feed entry's `<id>`/`<guid>`).
    pub external_id: Option<String>,
    /// Title to use only if neither the parser's Dublin Core title nor its
    /// fallback `ParsedDocument::title` yields one (applied strictly after
    /// that merge — see `process_url`).
    pub title_fallback: Option<String>,
    /// Author name(s) to stamp into `DublinCoreMetadata::creator` when
    /// non-empty (left as extracted by the parser when empty).
    pub creator: Vec<String>,
    /// Metadata date (RFC 3339) to stamp into `DublinCoreMetadata::date` when
    /// present. Never used for `Resource.added_at`/`modified_at` — see
    /// `modified_at_override` for the only enrichment that touches those.
    pub date: Option<String>,
    /// Value (RFC 3339) for `Resource.modified_at` when the source system
    /// carries its own modification timestamp (e.g. a feed entry's
    /// `updated`). `None` keeps the pre-enrichment behavior: `added_at` and
    /// `modified_at` are the same `now_rfc3339()` call. `added_at` is always
    /// ingestion-time `now()` regardless — it records when *our store* first
    /// saw the resource, not when the source last changed it.
    pub modified_at_override: Option<String>,
    /// Provenance source (e.g. the owning feed's URL) to stamp into
    /// `DublinCoreMetadata::source` when present.
    pub provenance_source: Option<String>,
    /// Whether to carry the fetch response's ETag into
    /// `Resource.external_etag`. `false` reproduces `UrlIngestor`'s current
    /// behavior of always leaving it `None`.
    pub capture_etag: bool,
}

/// Fetch, sniff, parse, and enrich a single locator into a `Resource`,
/// reporting through `callback`/`result` per the module-level contract.
///
/// `locator` and `uri` are deliberately both required: `UrlIngestor`
/// historically computes `resource_id` and the filename-sniff hint from the
/// *raw configured string*, not from the parsed `Uri` (they can differ, e.g.
/// in percent-encoding normalization) — see `url_ingestor.rs`'s prior
/// `resource_id(url, &hash)` and `url.split('/').next_back()` calls. This
/// helper preserves that quirk exactly rather than re-deriving either from
/// `uri`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_url(
    parser: &dyn Parser,
    fetcher: &dyn UrlFetcher,
    locator: &str,
    uri: &Uri,
    source: &IngestSource,
    kind: IngestorKind,
    enrich: &ResourceEnrichment,
    callback: &mut dyn IngestCallback,
    result: &mut IngestResult,
) -> Result<UrlOutcome, Error> {
    let fetch_meta = FetchMetadata::default();
    // Note: conditional-GET metadata is always the default here (no
    // previously-stored ETag/Last-Modified is threaded in) — a known gap,
    // marked with a `TODO` in `core::ingestion`.
    let fetch_result = match fetcher.fetch(locator, &fetch_meta).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(url = %locator, "process_url: fetch error: {}", e);
            // Report via on_skipped so the delete-sweep keeps this locator's
            // previously indexed content: a transient network failure is not
            // evidence the resource is gone (contrast with `Gone` below,
            // which stays silent precisely so the sweep deletes).
            callback
                .on_skipped(uri, SkipReason::Error(format!("fetch error: {e}")))
                .await;
            result.errors += 1;
            return Ok(UrlOutcome::FetchError);
        }
    };

    let (bytes, content_type, etag) = match fetch_result {
        FetchResult::Downloaded {
            bytes,
            content_type,
            etag,
            ..
        } => (bytes, content_type, etag),
        FetchResult::NotModified => {
            callback.on_skipped(uri, SkipReason::Unchanged).await;
            result.resources_skipped += 1;
            return Ok(UrlOutcome::Unchanged);
        }
        FetchResult::Gone => {
            // Confirmed absent (404/410 after retry). Do NOT yield a
            // Resource and do NOT call `on_skipped`: the pipeline's
            // delete-sweep treats every URI reported via
            // `on_resource`/`on_skipped` as still alive, and removes
            // indexed content only for URIs that were never reported.
            // Staying silent here is what gets this URI's chunks deleted.
            tracing::info!(url = %locator, "process_url: URL is gone (404/410)");
            return Ok(UrlOutcome::Gone);
        }
        FetchResult::Blocked => {
            // Mirrors the `Gone` arm structurally — log and return without a
            // callback, leaving the decision to the caller — but for a
            // different reason: the destination was refused before any
            // connection happened, so we know nothing about whether the
            // resource exists. Every caller must therefore report *something*
            // for this URI (see `UrlOutcome::Blocked`), or the delete-sweep
            // would read the silence as "gone" and delete content that is
            // very much still there.
            tracing::warn!(url = %locator, "process_url: destination blocked by fetch policy");
            return Ok(UrlOutcome::Blocked);
        }
    };

    let filename = locator.split('/').next_back().map(|s| s.to_string());
    // `sniff_mime` over bytes+filename feeds the parser chain's `Probe`, not
    // the HTTP `Content-Type` header (the parser chain never receives that
    // header either).
    let sniffed = sniff_mime(&bytes, filename.as_deref());
    let probe = Probe::new(&bytes, Some(locator), sniffed.as_deref());

    // Panic-tolerant parsing. A panic IS an error (matching the pipeline's
    // behavior of folding panics into the error count), so it's reported via
    // SkipReason::Error rather than a benign-skip counter.
    let parsed = match catch_panic(std::panic::AssertUnwindSafe(|| parser.parse(&probe))) {
        Err(panic_msg) => {
            tracing::warn!(url = %locator, "process_url: parser panicked: {}", panic_msg);
            callback.on_skipped(uri, SkipReason::Error(panic_msg)).await;
            result.errors += 1;
            return Ok(UrlOutcome::ParseFailed);
        }
        Ok(Ok(Some(doc))) => doc,
        Ok(Ok(None)) => {
            // Unsupported: deliberately NOT reported here — the caller
            // decides (immediate report for `UrlIngestor`, embedded-content
            // fallback attempt first for `FeedIngestor`).
            return Ok(UrlOutcome::Unsupported);
        }
        Ok(Err(e)) => {
            tracing::warn!(url = %locator, "process_url: parser error: {}", e);
            callback
                .on_skipped(uri, SkipReason::Error(format!("parser error: {e}")))
                .await;
            result.errors += 1;
            return Ok(UrlOutcome::ParseFailed);
        }
    };

    if parsed.markdown.trim().is_empty() {
        // The parser accepted the format and returned content, but that
        // content is empty (e.g. a 200 with an empty/whitespace-only body).
        // Deliberately NOT reported here — the caller decides, exactly like
        // `Unsupported` above (Codex review finding F1: a 0-block Resource
        // would otherwise reach `index_resource`'s empty-chunks arm and
        // silently delete any previously indexed content for this URI).
        tracing::info!(url = %locator, "process_url: extraction produced no content");
        return Ok(UrlOutcome::Empty);
    }

    let mut resource = build_resource(
        source,
        kind,
        uri,
        locator,
        &parsed.markdown,
        parsed.title.clone(),
        parsed.metadata.clone(),
        content_type,
        enrich,
    );
    resource.external_etag = if enrich.capture_etag { etag } else { None };

    callback.on_resource(resource).await?;
    result.resources_produced += 1;
    Ok(UrlOutcome::Indexed)
}

/// Assemble a `Resource` from already-extracted Markdown, applying the same
/// title-merge/enrichment/timestamp rules `process_url` uses after a fetch +
/// parse. Factored out so `FeedIngestor`'s embedded-content fallback (no
/// fetch involved — the Markdown comes from routing the feed entry's own
/// `content`/`summary`) produces byte-for-byte the same `Resource` shape as
/// the fetched-page path, rather than a hand-rolled duplicate.
///
/// `external_etag` is always `None` on the returned `Resource` — only
/// `process_url` has a fetch response to pull an ETag from; it patches the
/// field in afterward when `enrich.capture_etag` is set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_resource(
    source: &IngestSource,
    kind: IngestorKind,
    uri: &Uri,
    locator: &str,
    markdown: &str,
    parsed_title: Option<String>,
    parsed_metadata: DublinCoreMetadata,
    mime: Option<String>,
    enrich: &ResourceEnrichment,
) -> Resource {
    let blocks = markdown_to_blocks(markdown);
    let hash = compute_blocks_hash(&blocks);
    let res_id = resource_id(locator, &hash);
    let now = now_rfc3339();

    // Title merge: dc.title.or(parsed_title), THEN the enrichment's
    // title_fallback applies only if that merge still yields None.
    let mut dc = parsed_metadata;
    if dc.title.is_none() {
        dc.title = parsed_title;
    }
    if dc.title.is_none() {
        dc.title = enrich.title_fallback.clone();
    }
    let title = dc.title.clone();

    if !enrich.creator.is_empty() {
        dc.creator = enrich.creator.clone();
    }
    if let Some(date) = &enrich.date {
        dc.date = Some(date.clone());
    }
    if let Some(src) = &enrich.provenance_source {
        dc.source = Some(src.clone());
    }

    Resource {
        id: res_id,
        store_id: source.store_id.clone(),
        source_id: source.source_id.clone(),
        ingestor_kind: kind,
        resource_kind: ResourceKind::Document,
        uri: uri.clone(),
        external_id: enrich.external_id.clone(),
        external_etag: None,
        content_hash: hash,
        title,
        mime,
        metadata: Metadata::Document(DocumentMetadata {
            dublin_core: dc,
            ..Default::default()
        }),
        added_at: now.clone(),
        modified_at: enrich.modified_at_override.clone().unwrap_or(now),
        thread_id: None,
        channel: None,
        participants: vec![],
        origin_store: source.store_id.clone(),
        // Stamp the policy version the caller actually requested for this
        // run (not a hardcoded placeholder).
        policy_version: source.policy_version.clone(),
        share_path: None,
        extractor_version: "1.0".to_string(),
        blocks,
    }
}
