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
        // `url` sources capture the fetch response's ETag/Last-Modified onto
        // the indexed `Resource` (specs/04-search-pipeline.md §1), so a
        // later run's `lookup_fetch_metadata` has something to replay as
        // `If-None-Match`/`If-Modified-Since`.
        let enrichment = ResourceEnrichment {
            capture_conditional_get: true,
            ..Default::default()
        };

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
mod tests;
