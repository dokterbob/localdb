//! Feed ingestor: fetches an Atom/RSS/JSON feed, discovers entries, and
//! either follows each entry's link (discovery mode) or emits the whole feed
//! as a single document (single-doc mode) — issue #116.
//!
//! Built on the shared per-URL pipeline in [`crate::url_pipeline`]: each
//! entry's linked page is fetched/parsed/enriched exactly like a plain
//! `UrlIngestor` URL, with feed-derived enrichment (external id, author,
//! date, provenance) layered on top. See the module-level docs on
//! `url_pipeline` for the fetch/parse/report contract this reuses.
//!
//! feed-rs must be handed **raw bytes** — it decodes the charset from the
//! XML prolog/BOM itself. Callers must never pre-decode with
//! `String::from_utf8` first (verified against `feed-rs` 2.4.0 source: the
//! XML reader is configured with `quick_xml`'s `encoding` support).

use extract::html::extract_html;
use extract::plaintext::extract_plaintext;
use feed_rs::model::{Entry, Feed};
use localdb_core::block::IngestorKind;
use localdb_core::error::Error;
use localdb_core::ingestion::{FetchMetadata, FetchResult, UrlFetcher};
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor, SkipReason};
use localdb_core::metadata::DublinCoreMetadata;
use localdb_core::parser::Parser;
use localdb_core::uri::Uri;
use mediatype::MediaTypeBuf;

use crate::support::catch_panic;
use crate::url_pipeline::{build_resource, process_url, ResourceEnrichment, UrlOutcome};

/// Feed ingestor for `SourceSpec::Feed` sources (Atom, RSS 2.0/1.0/0.9x,
/// JSON Feed — whatever `feed-rs::parser` auto-detects).
pub struct FeedIngestor {
    parser: Box<dyn Parser>,
    feed_fetcher: Box<dyn UrlFetcher>,
    entry_fetcher: Box<dyn UrlFetcher>,
}

impl FeedIngestor {
    /// Create a new `FeedIngestor` with the given parser chain (used for
    /// entry pages in discovery mode) and two fetchers.
    ///
    /// The split is a trust boundary, not an optimization. `feed_fetcher`
    /// requests the *operator-configured* feed URL — the same trust class as a
    /// `url` source, so it gets the same unrestricted client; a homelab or LAN
    /// feed is a legitimate thing to configure. `entry_fetcher` requests entry
    /// `<link>`s, which are **attacker-controlled content** inside the feed
    /// document, so it must be a destination-restricted client (see
    /// `fetch::HttpUrlFetcher::new_public_only`). Restricting entry links only
    /// also degrades gracefully: an internal feed's entries fall back to their
    /// embedded summary rather than the whole source failing at step one.
    pub fn new(
        parser: Box<dyn Parser>,
        feed_fetcher: Box<dyn UrlFetcher>,
        entry_fetcher: Box<dyn UrlFetcher>,
    ) -> Self {
        Self {
            parser,
            feed_fetcher,
            entry_fetcher,
        }
    }
}

#[async_trait::async_trait]
impl Ingestor for FeedIngestor {
    fn kind(&self) -> IngestorKind {
        IngestorKind::Feed
    }

    async fn ingest(
        &self,
        source: &IngestSource,
        callback: &mut dyn IngestCallback,
    ) -> Result<IngestResult, Error> {
        let feed_url = match source.config.get("url").and_then(|v| v.as_str()) {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => {
                return Err(Error::InvalidRequest {
                    message: "FeedIngestor: missing required config field 'url'".to_string(),
                })
            }
        };
        let max_entries = source
            .config
            .get("max_entries")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let fetch_full_content = source
            .config
            .get("fetch_full_content")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Fail-fast, before any I/O and before `on_discovered` — mirrors
        // `url_ingestor.rs`'s hoisted `Uri::parse` fail-fast pattern exactly
        // (a single `Error::Internal`, whole run aborted as one unit).
        let feed_uri = Uri::parse(&feed_url).ok_or_else(|| Error::Internal {
            message: format!("FeedIngestor: invalid URI '{}'", feed_url),
            correlation_id: "feed_ingestor_uri".to_string(),
        })?;

        let mut result = IngestResult::default();

        let fetch_result = match self
            .feed_fetcher
            .fetch(&feed_url, &FetchMetadata::default())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // Batch semantics: a multi-source run continues past a
                // single feed's fetch failure.
                // Debug, not warn: `core::ingestion` emits the single
                // user-facing WARN for every SkipReason::Error. The blocked
                // arm below stays at warn — it reports `SkipReason::Other`,
                // for which `core` logs nothing.
                tracing::debug!(url = %feed_url, "FeedIngestor: feed fetch error: {}", e);
                callback
                    .on_skipped(
                        &feed_uri,
                        SkipReason::Error(format!("feed fetch error: {e}")),
                    )
                    .await;
                result.errors += 1;
                return Ok(result);
            }
        };

        // `effective_feed_url` is ONLY the parser's link-resolution base —
        // never substitute it for `feed_url` anywhere else. `feed_url`
        // remains the value used for `feed_uri` (source identity),
        // `synthetic_entry_uri`'s `{feed_url}#entry:{id}` fragments, every
        // `enrichment.provenance_source`, and all tracing fields: keeping
        // those pinned to the *configured* URL is what stops a transient
        // redirect-target change from re-keying every link-less entry (see
        // specs/02-domain-model.md's "Feed connector" section).
        let (bytes, effective_feed_url) = match fetch_result {
            FetchResult::Downloaded {
                bytes, final_url, ..
            } => (bytes, final_url.unwrap_or_else(|| feed_url.clone())),
            FetchResult::NotModified => {
                // The "feed 304 => zero per-entry callbacks" case the core
                // sweep exemption for `SourceSpec::Feed` protects: a single
                // `Unchanged` skip, no entry-level callbacks at all.
                callback.on_skipped(&feed_uri, SkipReason::Unchanged).await;
                result.resources_skipped += 1;
                return Ok(result);
            }
            FetchResult::Gone => {
                // Silent, like `process_url`'s Gone handling: no callback at
                // all. For a feed source that silence does *not* cause a
                // deletion, though — `run_source_ingestion` exempts every
                // `SourceSpec::Feed` source from the delete-sweep (see
                // core/src/ingestion.rs), so the run reports zero errors, zero
                // skips and zero produced, and whatever this feed indexed
                // previously stays until `source remove`.
                //
                // That is deliberate. Dropping a whole source's content on a
                // single 404 is irreversible, and a 404 on a feed root is
                // frequently transient (misconfiguration, migration, CDN) —
                // exactly the "unavailable != empty" tension in issue #156.
                // Retention is spec'd in specs/02-domain-model.md's "Feed
                // connector" / "Retention" paragraph and tracked as known gap
                // 8 in docs/architecture.md; the pruning policy that would let a
                // persistently-gone feed be reclaimed belongs to issue #171
                // (persist conditional-GET state and prune on 404/410).
                tracing::info!(url = %feed_url, "FeedIngestor: feed is gone (404/410)");
                return Ok(result);
            }
            FetchResult::Blocked => {
                // Unreachable under the current wiring: `feed_fetcher` is the
                // unrestricted client by construction (the feed URL is
                // operator-configured — see `FeedIngestor::new`), so it never
                // returns `Blocked`. The arm exists because `FetchResult` is
                // exhaustively matched, and it must not silently vanish if a
                // future caller ever hands this ingestor a restricted feed
                // fetcher.
                //
                // Treated exactly like `Gone`: silent, no callback. The
                // sweep-exemption for `SourceSpec::Feed` means that silence
                // preserves whatever the feed indexed previously, which is
                // the right outcome for a refusal that says nothing about
                // whether the feed still exists.
                tracing::warn!(url = %feed_url, "FeedIngestor: feed destination blocked by fetch policy");
                return Ok(result);
            }
        };

        // Panic-tolerant parse of raw bytes. feed-rs resolves relative entry
        // links itself when given `base_uri` (xml:base takes precedence) —
        // never re-resolve links in this ingestor. The base is the
        // *effective* (post-redirect) feed URL, not the configured
        // `feed_url`: a feed that 301's to a new host must resolve its
        // entries' relative links against that new host (Codex review
        // finding F2). This is purely about where a relative link points —
        // identity (`feed_uri`, fragment URIs, provenance) stays pinned to
        // `feed_url` regardless, per the comment above.
        //
        // The id generator is the one place the effective URL must NOT leak.
        // feed-rs synthesizes an id for any entry that declares none, and for
        // a link-less entry it derives that id from the parser's base URI plus
        // the title — which would put the effective URL inside
        // `synthetic_entry_uri`'s `{feed_url}#entry:{id}`, moving the whole
        // Resource identity the first time a feed starts redirecting (old URI
        // delete-swept, same entry re-indexed under a new one). Pinning the
        // generator's URI argument to the configured `feed_url` keeps that
        // stable without affecting link resolution: `generate_id` only
        // consults the argument on the link-less branch, and for a linked
        // entry it hashes the (correctly, effectively-resolved) link instead.
        // `feed_rs::parser::Parser::parse` is CPU-bound (XML/JSON parsing of
        // the whole feed body); this may run under the daemon's shared
        // HTTP/SSE-serving tokio runtime (issue #187 real ingestion), so it's
        // guarded with `run_blocking` rather than called inline — see
        // `core::blocking::run_blocking`'s doc comment for why that's
        // `block_in_place`-on-multi-thread rather than a bare call.
        let id_generator_url = feed_url.clone();
        let parse_outcome = localdb_core::run_blocking(|| {
            catch_panic(std::panic::AssertUnwindSafe(|| {
                feed_rs::parser::Builder::new()
                    .base_uri(Some(effective_feed_url.as_str()))
                    .id_generator(move |links, title, _effective_uri| {
                        feed_rs::parser::generate_id(links, title, Some(id_generator_url.as_str()))
                    })
                    .build()
                    .parse(bytes.as_slice())
            }))
        });
        let mut feed: Feed = match parse_outcome {
            Err(panic_msg) => {
                // Debug: `core::ingestion` owns the user-facing WARN. The
                // "panicked" framing goes into the payload, which `core`
                // prints verbatim — otherwise a crash is indistinguishable
                // from an ordinary returned Err at the default log level.
                tracing::debug!(url = %feed_url, "FeedIngestor: feed parser panicked: {}", panic_msg);
                callback
                    .on_skipped(
                        &feed_uri,
                        SkipReason::Error(format!("feed parser panicked: {panic_msg}")),
                    )
                    .await;
                result.errors += 1;
                return Ok(result);
            }
            Ok(Err(e)) => {
                // Debug: `core::ingestion` owns the user-facing WARN.
                tracing::debug!(url = %feed_url, "FeedIngestor: feed parse error: {}", e);
                callback
                    .on_skipped(
                        &feed_uri,
                        SkipReason::Error(format!("feed parse error: {e}")),
                    )
                    .await;
                result.errors += 1;
                return Ok(result);
            }
            Ok(Ok(feed)) => feed,
        };

        // `mem::take`, not `.clone()`: a large feed's entries would otherwise
        // exist twice at peak. Only `entries` is moved out — `feed.title` /
        // `feed.description` / `feed.updated` remain usable below (single-doc
        // mode needs them).
        let mut entries = std::mem::take(&mut feed.entries);

        // Atom's inheritance rule: a feed-level `<author>` is the effective
        // author of every entry that declares none (RFC 4287 §4.2.1). Computed
        // once here rather than per entry — `Feed.authors` and `Entry.authors`
        // are the same `Vec<Person>`, so `person_display` applies unchanged.
        let feed_authors: Vec<String> = feed.authors.iter().filter_map(person_display).collect();

        // Stable-sort DESC by published.or(updated); `None` dates sort last.
        // Never `sort_unstable_by` — and always sort BEFORE truncating: an
        // oldest-first archive feed would otherwise permanently pin the
        // wrong N entries (document order != recency for such feeds).
        entries.sort_by(|a, b| {
            let key_a = a.published.or(a.updated);
            let key_b = b.published.or(b.updated);
            match (key_a, key_b) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });
        // Deduplicate by resolved resource URI — the same URI that becomes
        // the eventual Resource's identity (see the "Both modes" paragraph
        // of specs/02-domain-model.md's Feed connector section) — keeping
        // the first (i.e. newest, since `entries` is already sorted DESC
        // above) occurrence of each and dropping the rest, BEFORE
        // `max_entries` truncation, so `max_entries` counts *distinct*
        // resource URIs, not raw entry count (Codex review finding F3: two
        // entries resolving to the same URI used to both be processed,
        // burning two slots and — on the embedded-content fallback path —
        // letting the older entry's body win over the newer one since it
        // was processed last). `Vec::dedup_by` is deliberately NOT used
        // here: it only collapses *adjacent* duplicates, and this vec is
        // sorted by date, not by URI, so duplicate entries are not
        // guaranteed to be neighbours.
        //
        // Link-less entries resolve to an `entry.id`-derived synthetic
        // fragment URI (`synthetic_entry_uri`), which differs per entry in
        // virtually every real feed, so this pass is a no-op for them in
        // practice — except in the pathological case of a feed repeating
        // the same `<guid>` across entries, where collapsing them down to
        // the newest is also the correct outcome.
        //
        // The resolved `(locator, uri, fetchable)` triple is threaded
        // straight into the discovery loop below so it is computed exactly
        // once per surviving entry, not recomputed inside
        // `process_discovery_entry`.
        let mut seen_uris: std::collections::HashSet<Uri> = std::collections::HashSet::new();
        let mut targets: Vec<(String, Uri, bool)> = Vec::with_capacity(entries.len());
        entries.retain(|entry| {
            let target = resolve_entry_target(entry, &feed_url, &feed_uri);
            let is_new = seen_uris.insert(target.1.clone());
            if is_new {
                targets.push(target);
            }
            is_new
        });

        if let Some(max) = max_entries {
            entries.truncate(max);
            targets.truncate(max);
        }

        if fetch_full_content {
            callback.on_discovered(entries.len()).await;
            for (entry, (locator, uri, fetchable)) in entries.iter().zip(targets.iter()) {
                process_discovery_entry(
                    self.parser.as_ref(),
                    // Entry links are third-party content: the restricted
                    // fetcher, never `feed_fetcher`. See `FeedIngestor::new`.
                    self.entry_fetcher.as_ref(),
                    entry,
                    locator,
                    uri,
                    *fetchable,
                    &feed_url,
                    &feed_authors,
                    source,
                    callback,
                    &mut result,
                )
                .await?;
            }
        } else {
            callback.on_discovered(1).await;
            let feed_title_text = feed_title_or_default(&feed);
            let markdown = build_single_doc_markdown(&feed, &feed_title_text, &entries);
            // `modified_at` comes from the feed when it says anything:
            // `feed.updated`, else the newest entry's date (entries are
            // already sorted DESC on published.or(updated), so that's the
            // first entry's sort key), else ingestion-time now().
            let modified_at_override = feed
                .updated
                .or_else(|| entries.first().and_then(|e| e.published.or(e.updated)))
                .map(|d| d.to_rfc3339());
            let enrichment = ResourceEnrichment {
                external_id: None,
                title_fallback: Some(feed_title_text),
                creator: Vec::new(),
                date: None,
                modified_at_override,
                provenance_source: Some(feed_url.clone()),
                capture_etag: false,
            };
            let resource = build_resource(
                source,
                IngestorKind::Feed,
                &feed_uri,
                &feed_url,
                &markdown,
                None,
                DublinCoreMetadata::default(),
                Some("text/markdown".to_string()),
                &enrichment,
            );
            callback.on_resource(resource).await?;
            result.resources_produced += 1;
        }

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Discovery mode — per-entry processing
// ---------------------------------------------------------------------------

/// Human-usable display string for a feed-rs `Person`, or `None` if it has
/// nothing usable. feed-rs hardcodes `Person.name` to the literal string
/// `"author"` for RSS 2.0 `<author>` elements and puts the element's actual
/// text (an email, possibly `email (Name)`) in `Person.email` (verified
/// against feed-rs `parser/rss2/mod.rs::handle_contact`) — so that
/// placeholder must never leak into creator/byline output; the email field
/// is the real value there.
fn person_display(p: &feed_rs::model::Person) -> Option<String> {
    let name = p.name.trim();
    if !name.is_empty() && name != "author" {
        return Some(name.to_string());
    }
    p.email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(String::from)
}

/// The authors that apply to `entry`, honouring Atom's inheritance rule: a
/// feed-level `<author>` is the effective author of any entry that declares
/// none (RFC 4287 §4.2.1). An entry's own authors always win outright — the
/// two lists are never merged.
///
/// The emptiness test is on the *displayable* names, not on `entry.authors`
/// itself, so an entry carrying only unusable `Person` records (feed-rs's
/// `"author"` placeholder with no email — see [`person_display`]) still
/// inherits rather than ending up with no byline at all.
fn effective_authors(entry: &Entry, feed_authors: &[String]) -> Vec<String> {
    let own: Vec<String> = entry.authors.iter().filter_map(person_display).collect();
    if own.is_empty() {
        feed_authors.to_vec()
    } else {
        own
    }
}

/// First entry link with `rel` `None` or `"alternate"` (RSS `<link>` maps to
/// `rel: None`; Atom defaults an omitted `rel` to `"alternate"` itself —
/// verified against feed-rs source, `parser/atom/mod.rs`'s `handle_link`).
fn select_entry_link(entry: &Entry) -> Option<&feed_rs::model::Link> {
    entry
        .links
        .iter()
        .find(|l| l.rel.is_none() || l.rel.as_deref() == Some("alternate"))
}

/// Build the synthetic fragment URI used for link-less entries, or as the
/// resource locator for an unparseable entry link. Entry links are untrusted
/// data — tolerate a bad one, never abort the run over it.
///
/// # Entry-id churn (accepted, by design)
/// feed-rs assigns `entry.id` in this priority order when the source XML
/// omits `<id>`/`<guid>`: (1) `SipHash128(link.href, title)` if a link is
/// present, (2) `SipHash128(base_uri, title)` if a `base_uri` and title are
/// both available, (3) a random UUID as a last resort (verified against
/// feed-rs `parser/mod.rs::generate_id`). For a link-less entry with no
/// title either, every run mints a fresh random UUID and this fragment URI
/// churns on every run — that's an accepted limitation of the fallback path
/// (no per-entry identity from the source to anchor to). A link-less entry
/// WITH a title also churns whenever the title text is edited, since the
/// hash includes the title. This is unavoidable without a stored id and is
/// intentionally not treated as an error.
fn synthetic_entry_uri(feed_url: &str, feed_uri: &Uri, entry_id: &str) -> Uri {
    let candidate = format!("{feed_url}#entry:{entry_id}");
    Uri::parse(&candidate).unwrap_or_else(|| feed_uri.clone())
}

/// Resolve an entry's `(locator, resource URI, fetchable)` triple. Hoisted
/// out of `process_discovery_entry` (Codex review finding F3) so the
/// dedup-by-resolved-URI pass in `ingest` can key on the same resolution
/// before entries are ever handed to per-entry processing — this is a pure
/// refactor, the resolution logic itself is unchanged.
fn resolve_entry_target(entry: &Entry, feed_url: &str, feed_uri: &Uri) -> (String, Uri, bool) {
    let link_href = select_entry_link(entry).map(|l| l.href.clone());
    // A parsed link is the resource identity either way (stable, matching
    // the "URI keys off the feed-declared link" contract), but only an
    // http(s) one is *fetchable*: `Uri::parse` happily accepts `mailto:` /
    // `ftp:` links, and handing those to the HTTP fetcher would fail as a
    // transient FetchError every run — which never falls back, so the
    // entry's embedded content would never be indexed at all.
    match link_href.as_deref().map(Uri::parse) {
        Some(Some(parsed)) => {
            let fetchable = matches!(parsed.scheme(), "http" | "https");
            (link_href.clone().unwrap(), parsed, fetchable)
        }
        // Absent or unparseable link -> synthetic fragment URI. Untrusted
        // entry data must never abort the run.
        _ => {
            let frag = format!("{feed_url}#entry:{}", entry.id);
            let u = synthetic_entry_uri(feed_url, feed_uri, &entry.id);
            (frag, u, false)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn process_discovery_entry(
    parser: &dyn Parser,
    fetcher: &dyn UrlFetcher,
    entry: &Entry,
    locator: &str,
    uri: &Uri,
    fetchable: bool,
    feed_url: &str,
    feed_authors: &[String],
    source: &IngestSource,
    callback: &mut dyn IngestCallback,
    result: &mut IngestResult,
) -> Result<(), Error> {
    let enrichment = ResourceEnrichment {
        external_id: Some(entry.id.clone()),
        title_fallback: entry.title.as_ref().map(|t| t.content.clone()),
        creator: effective_authors(entry, feed_authors),
        date: entry.published.or(entry.updated).map(|d| d.to_rfc3339()),
        // The feed's own modification claim, preferring `updated` (that's
        // what it means) over `published`; `dc.date` above keeps the
        // opposite preference (creation/publication semantics).
        modified_at_override: entry.updated.or(entry.published).map(|d| d.to_rfc3339()),
        provenance_source: Some(feed_url.to_string()),
        capture_etag: true,
    };

    let needs_fallback = if fetchable {
        // Asymmetric fallback (pinned): transient failures (FetchError,
        // ParseFailed) already reported themselves as errors — no fallback,
        // so the last good index stays put instead of flip-flopping between
        // full-page and summary content on every transient hiccup.
        // `Gone`/`Unsupported`/`Empty` are all stable properties of the
        // linked page (a 404 stays a 404, an unhandled format stays
        // unhandled, and a page that renders to nothing renders to nothing
        // again next run), so those DO fall back to the entry's own
        // embedded content. `Empty` in particular (Codex review finding F1)
        // must never be indexed as-is: a 0-block Resource with a changed
        // content hash would hit `index_resource`'s empty-chunks arm and
        // delete any previously indexed content for this URI — unlike a
        // transient fetch/parse failure, there's no flip-flop risk here
        // because emptiness is stable, so falling back to the entry's own
        // summary/title is safe and strictly better than reporting nothing.
        //
        // `Blocked` joins that stable set for the same reason and is the
        // whole of the entry-level SSRF fix's user-visible behavior: a
        // destination refused by the guard will be refused identically next
        // run, so falling back to the entry's embedded content is both safe
        // and the graceful-degradation path that lets an internal feed keep
        // working (entries indexed from their summaries, links never
        // fetched). It also means the URI IS reported — via `on_resource`
        // here, or `on_skipped(Error)` if the entry carries no usable content
        // at all — which is what keeps the delete-sweep contract intact.
        let outcome = process_url(
            parser,
            fetcher,
            locator,
            uri,
            source,
            IngestorKind::Feed,
            &enrichment,
            callback,
            result,
        )
        .await?;
        matches!(
            outcome,
            UrlOutcome::Gone | UrlOutcome::Unsupported | UrlOutcome::Empty | UrlOutcome::Blocked
        )
    } else {
        // Link-less and non-http(s)-linked entries never fetch — straight
        // to embedded content.
        true
    };

    if needs_fallback {
        match embedded_content_for_entry(entry) {
            Some((markdown, extracted_title, mime)) => {
                let resource = build_resource(
                    source,
                    IngestorKind::Feed,
                    uri,
                    locator,
                    &markdown,
                    extracted_title,
                    DublinCoreMetadata::default(),
                    mime,
                    &enrichment,
                );
                callback.on_resource(resource).await?;
                result.resources_produced += 1;
            }
            None => {
                callback
                    .on_skipped(uri, SkipReason::Error("feed entry: no usable content (no fetchable link, no content, no summary, no title)".to_string()))
                    .await;
                result.errors += 1;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Embedded-content routing (pinned table, verified against feed-rs source)
// ---------------------------------------------------------------------------

/// Route a feed-rs `Text`/`Content` payload's declared media type to the
/// right extractor. `None` means "not a piece we can meaningfully route"
/// (e.g. base64-encoded binary Atom content) — the caller falls through to
/// the next piece in the content -> summary -> title-only chain.
///
/// Facts verified directly against `feed-rs` 2.4.0 source
/// (`src/parser/{atom,rss2}/mod.rs`): RSS `<description>` and
/// `<content:encoded>` are unconditionally typed `text/html` (hardcoded, no
/// sniffing). Atom `<summary>` with no `type` attribute defaults to
/// `text/plain`; Atom `<content>` with no `type` attribute ALSO defaults to
/// `text/plain` when no `src` attribute is present (only the `src`-present,
/// `type`-absent combination defaults to `text/html`, since `body` is `None`
/// in that case anyway and gets skipped as absent regardless). CDATA is
/// transparent to feed-rs. Base64 bodies are never decoded by feed-rs — they
/// pass through as opaque text under whatever type the source declared
/// (typically `application/octet-stream`), which falls into the `_` arm
/// here and is skipped.
fn route_text(content_type: &MediaTypeBuf, body: &str) -> Option<(String, Option<String>)> {
    let essence = content_type.essence().to_string().to_ascii_lowercase();
    match essence.as_str() {
        "text/html" | "application/xhtml+xml" => extract_html(body).ok(),
        "text/plain" => extract_plaintext(body).ok(),
        s if s == "text/xml" || s == "application/xml" || s.ends_with("+xml") => {
            // scraper (used by extract_html) mishandles arbitrary XML, so
            // XML-typed pieces go through the plaintext extractor instead.
            extract_plaintext(body).ok()
        }
        _ => None,
    }
}

/// `entry.content` (if it has a body — `<content src=...>` has `body: None`
/// and is treated as absent, never fetched) then `entry.summary`, routed by
/// declared type. Returns `(markdown, extracted_title, mime_essence)`.
///
/// A piece whose extracted Markdown trims empty is unusable and falls
/// through to the next piece in the chain, exactly like an unroutable type:
/// a 0-block Resource with a changed content hash reaches core's
/// `index_resource` empty-chunks arm, which *deletes* the previously indexed
/// document — an entry whose `<content>` extracts to nothing must fall back
/// to its summary/title instead of erasing them.
fn entry_routed_content(entry: &Entry) -> Option<(String, Option<String>, String)> {
    if let Some(content) = &entry.content {
        if let Some(body) = &content.body {
            if let Some((md, title)) = route_text(&content.content_type, body) {
                if !md.trim().is_empty() {
                    return Some((md, title, content.content_type.essence().to_string()));
                }
            }
        }
    }
    if let Some(summary) = &entry.summary {
        if let Some((md, title)) = route_text(&summary.content_type, &summary.content) {
            if !md.trim().is_empty() {
                return Some((md, title, summary.content_type.essence().to_string()));
            }
        }
    }
    None
}

/// Full discovery-mode fallback chain: `entry.content` -> `entry.summary` ->
/// title-only Resource (valid: a Resource whose only content is its title)
/// -> `None` (caller reports `on_skipped(Error)`).
///
/// Enclosures (`entry.media`) are ignored entirely — not part of this chain.
fn embedded_content_for_entry(entry: &Entry) -> Option<(String, Option<String>, Option<String>)> {
    if let Some((md, title, mime)) = entry_routed_content(entry) {
        return Some((md, title, Some(mime)));
    }
    if let Some(t) = &entry.title {
        let text = t.content.trim();
        if !text.is_empty() {
            return Some((text.to_string(), None, None));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Single-document mode
// ---------------------------------------------------------------------------

fn feed_title_or_default(feed: &Feed) -> String {
    feed.title
        .as_ref()
        .map(|t| t.content.clone())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Untitled Feed".to_string())
}

/// Deterministic Markdown template for single-document mode. Exactness
/// matters: this is content-hashed, and the hash drives incremental-skip
/// decisions.
///
/// # Hash-stability residual risk (accepted)
/// The per-entry body is whatever `entry.content`/`entry.summary` HTML
/// happens to contain verbatim (after extraction to Markdown) — rotating ad
/// snippets, embedded "generated at <timestamp>" markers, or similar
/// publisher-side churn inside that HTML will still change the resulting
/// content hash and trigger re-indexing, even though nothing meaningful
/// changed. This is accepted: there is no general way to detect and strip
/// such noise without a publisher-specific heuristic.
fn build_single_doc_markdown(feed: &Feed, feed_title_text: &str, entries: &[Entry]) -> String {
    let mut md = format!("# {feed_title_text}\n\n");
    let feed_authors: Vec<String> = feed.authors.iter().filter_map(person_display).collect();

    if let Some(desc) = &feed.description {
        if let Some((body, _)) = route_text(&desc.content_type, &desc.content) {
            let body = body.trim();
            if !body.is_empty() {
                md.push_str(body);
                md.push_str("\n\n");
            }
        }
    }

    for entry in entries {
        let entry_title = entry
            .title
            .as_ref()
            .map(|t| t.content.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Untitled Entry".to_string());
        md.push_str(&format!("## {entry_title}\n\n"));

        // Byline: `*By {authors} — {date} — {link}*`, omitting missing
        // parts and their separators — no placeholders. Deliberately NOT
        // including entry.id/guid: feed-rs auto-ids churn on title edits
        // (see `synthetic_entry_uri`'s doc comment), which would make this
        // template — and therefore the whole single-doc content hash —
        // churn on every title tweak even when nothing else changed.
        let mut byline_parts: Vec<String> = Vec::new();
        // Same Atom inheritance rule discovery mode applies (`feed.authors`
        // is already in scope here — `feed` is a parameter — so no signature
        // change was needed).
        let author_names = effective_authors(entry, &feed_authors);
        if !author_names.is_empty() {
            byline_parts.push(format!("By {}", author_names.join(", ")));
        }
        if let Some(date) = entry.published.or(entry.updated) {
            byline_parts.push(date.to_rfc3339());
        }
        if let Some(link) = select_entry_link(entry) {
            byline_parts.push(link.href.clone());
        }
        if !byline_parts.is_empty() {
            md.push('*');
            md.push_str(&byline_parts.join(" — "));
            md.push_str("*\n\n");
        }

        if let Some((body, _, _)) = entry_routed_content(entry) {
            let body = body.trim();
            if !body.is_empty() {
                md.push_str(body);
                md.push_str("\n\n");
            }
        }
    }

    md
}

#[cfg(test)]
mod tests;
