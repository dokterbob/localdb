//! File-system ingestor: scans a directory tree, parses each file, and emits
//! typed [`Resource`]s.
//!
//! The CLI's concrete [`Ingestor`] for `path`-kind sources (issue #117):
//! progress hooks, mtime/mime handling, panic-tolerant parsing, and title
//! merge are all expressed through the `Ingestor`/`IngestCallback` contract.

use extract::sniff_mime;
use localdb_core::block::{IngestorKind, Resource, ResourceKind};
use localdb_core::error::Error;
use localdb_core::ids::resource_id;
use localdb_core::ingestion::{enumerate_path_source, now_rfc3339, PathEnumeration};
use localdb_core::ingestor::{
    Enumeration, IngestCallback, IngestResult, IngestSource, Ingestor, SkipReason,
};
use localdb_core::markdown_blocks::{compute_blocks_hash, markdown_to_blocks_with_pages};
use localdb_core::metadata::{DocumentMetadata, Metadata};
use localdb_core::parser::{Parser, Probe};

use crate::support::{catch_panic, detect_mime, format_unix_secs};

/// File-system ingestor.
///
/// Reads a directory tree from `source.config["root"]`, optionally filtered by
/// `source.config["include"]` (array of glob patterns) and
/// `source.config["exclude"]` (array of glob patterns), via
/// `core::ingestion::enumerate_path_source`.
pub struct FileIngestor {
    /// The parser chain to use for format detection and extraction.
    pub parser: Box<dyn Parser>,
}

impl FileIngestor {
    /// Create a new `FileIngestor` with the given parser chain.
    pub fn new(parser: Box<dyn Parser>) -> Self {
        Self { parser }
    }
}

#[async_trait::async_trait]
impl Ingestor for FileIngestor {
    fn kind(&self) -> IngestorKind {
        IngestorKind::File
    }

    async fn ingest(
        &self,
        source: &IngestSource,
        callback: &mut dyn IngestCallback,
    ) -> Result<IngestResult, Error> {
        // Extract configuration from the JSON config.
        let root = source
            .config
            .get("root")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::InvalidRequest {
                message: "FileIngestor: missing required config field 'root'".to_string(),
            })?;

        let include: Vec<String> = source
            .config
            .get("include")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let exclude: Vec<String> = source
            .config
            .get("exclude")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // `enumerate_path_source` owns directory-walk, hidden-file, extension
        // and glob filtering behavior (shared with any other path-source caller).
        //
        // The walk is blocking filesystem I/O (`std::fs::read_dir` recursion);
        // this may run under the daemon's shared HTTP/SSE-serving tokio
        // runtime (issue #187 real ingestion), so it's guarded with
        // `run_blocking` rather than called inline — see
        // `core::blocking::run_blocking`'s doc comment for why that's
        // `block_in_place`-on-multi-thread rather than a bare call.
        let files =
            match localdb_core::run_blocking(|| enumerate_path_source(root, &include, &exclude))? {
                PathEnumeration::Complete(files) => files,
                PathEnumeration::RootUnavailable => {
                    // #156: the root isn't there — an unmounted volume, a detached
                    // external disk, a directory that moved. We have observed
                    // nothing about this source's contents, which is *not* the
                    // same as observing that it is empty. Reporting
                    // `Enumeration::Incomplete` is what stops
                    // `run_source_ingestion`'s delete-sweep from reading our zero
                    // resources as "every document in this source was deleted."
                    tracing::warn!(
                        root = %root,
                        "source root is not reachable — enumerating nothing this run"
                    );
                    callback.on_discovered(0).await;
                    return Ok(IngestResult {
                        enumeration: Enumeration::Incomplete {
                            reason: format!("source root is not reachable: {root}"),
                        },
                        ..Default::default()
                    });
                }
            };

        // Signal `Discovered { total }` right after enumeration and before
        // processing the first file.
        callback.on_discovered(files.len()).await;

        let mut result = IngestResult::default();

        for file in &files {
            // Read + mtime in one `run_blocking` hop per file: both are
            // blocking filesystem I/O (`std::fs::read`, `Path::metadata`),
            // and this may run under the daemon's shared HTTP/SSE-serving
            // tokio runtime (issue #187 real ingestion) — see
            // `core::blocking::run_blocking`'s doc comment for why that's
            // `block_in_place`-on-multi-thread rather than a bare call.
            // mtime -> modified_at, formatted as RFC 3339 (falls back to
            // "now" if the filesystem doesn't report a modified time);
            // `added_at` is stamped separately below from the ingestion
            // clock, not from this value — only computed once the read
            // succeeds, matching the original sequencing.
            let (bytes, mtime_rfc3339) = match localdb_core::run_blocking(
                || -> Result<(Vec<u8>, String), std::io::Error> {
                    let bytes = std::fs::read(&file.path)?;
                    let mtime_rfc3339 = file
                        .path
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(|t| {
                            let secs = t
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            format_unix_secs(secs)
                        })
                        .unwrap_or_else(now_rfc3339);
                    Ok((bytes, mtime_rfc3339))
                },
            ) {
                Ok(v) => v,
                Err(e) => {
                    // Debug, not warn: `core::ingestion` emits the single
                    // user-facing WARN for every SkipReason::Error (it owns
                    // ingestion outcome accounting). This line keeps the
                    // extra framing for troubleshooting without duplicating
                    // the warning.
                    tracing::debug!(path = %file.path.display(), "FileIngestor: failed to read file: {}", e);
                    // Report via on_skipped so the delete-sweep keeps this
                    // still-existing file's indexed content: only URIs never
                    // reported at all get swept, and a transient read error
                    // must not delete good chunks. SkipReason::Error (not
                    // Other) so the pipeline counts this as an error rather
                    // than a benign skip (C8).
                    callback
                        .on_skipped(&file.uri, SkipReason::Error(format!("read error: {e}")))
                        .await;
                    result.errors += 1;
                    continue;
                }
            };

            let filename = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string());

            // Two distinct mime computations:
            //  - `detect_mime` (extension-based) is what gets stamped onto
            //    the stored document/chunk metadata.
            //  - `extract::sniff_mime` (magic bytes + extension) feeds into
            //    `Probe.sniffed_mime` before calling the parser chain.
            let mime = detect_mime(&file.path);
            let sniffed = sniff_mime(&bytes, filename.as_deref());
            // `Path::to_str()` returns `None` if *any* component of the path
            // (not just the filename) is non-UTF-8 — e.g. a valid `notes.md`
            // living under a non-UTF-8-named directory. Falling back to
            // `None` here would blind extension-gated parsers (which read
            // `Probe::path_hint` for the extension) and misclassify a
            // perfectly supported file as `SkipReason::Unsupported`. Fall
            // back to a lossy hint instead — it's only used for
            // extension/mime sniffing, never persisted, so lossy
            // replacement characters are harmless.
            let path_hint = path_hint_lossy(&file.path);
            let probe = Probe::new(&bytes, Some(path_hint.as_str()), sniffed.as_deref());

            // Panic-tolerant parsing: a panicking parser must not crash the
            // whole walk. `catch_panic` wraps extraction and the panic is
            // surfaced via `on_skipped` + `SkipReason::Error` (a panic IS an
            // error, matching the old pipeline's behavior of folding panics
            // into the error count, C8) rather than the benign-skip counter.
            //
            // `Parser::parse` is documented sync/CPU-bound (`core::parser`);
            // this may run under the daemon's shared HTTP/SSE-serving tokio
            // runtime (issue #187 real ingestion), so it's guarded with
            // `run_blocking` rather than called inline — see
            // `core::blocking::run_blocking`'s doc comment for why that's
            // `block_in_place`-on-multi-thread rather than a bare call.
            let parsed = match localdb_core::run_blocking(|| {
                catch_panic(std::panic::AssertUnwindSafe(|| self.parser.parse(&probe)))
            }) {
                Err(panic_msg) => {
                    // Debug: `core::ingestion` owns the user-facing WARN.
                    tracing::debug!(uri = %file.uri, "FileIngestor: parser panicked: {}", panic_msg);
                    // The "parser panicked" framing must live in the payload,
                    // not only in the debug line above: `core`'s single WARN
                    // prints the payload verbatim, and without this a crash
                    // and an ordinary returned Err are indistinguishable at
                    // the default log level. The read/parse-error arms
                    // already prefix theirs for the same reason.
                    callback
                        .on_skipped(
                            &file.uri,
                            SkipReason::Error(format!("parser panicked: {panic_msg}")),
                        )
                        .await;
                    result.errors += 1;
                    continue;
                }
                Ok(Ok(Some(doc))) => doc,
                Ok(Ok(None)) => {
                    callback
                        .on_skipped(&file.uri, SkipReason::Unsupported)
                        .await;
                    result.resources_skipped += 1;
                    continue;
                }
                Ok(Err(e)) => {
                    // Debug: `core::ingestion` owns the user-facing WARN.
                    tracing::debug!(uri = %file.uri, "FileIngestor: parser error: {}", e);
                    // Same aliveness rule as the read-error path above;
                    // SkipReason::Error so it's counted as an error (C8).
                    callback
                        .on_skipped(&file.uri, SkipReason::Error(format!("parser error: {e}")))
                        .await;
                    result.errors += 1;
                    continue;
                }
            };

            // Page stamping (#103): `page_starts` is empty for non-paginated
            // formats, in which case this is plain `markdown_to_blocks`.
            let blocks = markdown_to_blocks_with_pages(&parsed.markdown, &parsed.page_starts);

            // #185, defense in depth: never yield a contentless `Resource`.
            // The parser accepted this file and returned something, but it
            // extracted to nothing usable — a whitespace-only file, a scanned
            // PDF with no text layer, an HTML page whose body is all script.
            // Yielding that as a `Resource` would be claiming "here is this
            // document's content" on no evidence. The sink refuses empty
            // replacements too (`core::ingestion::index_resource`), but an
            // ingestor should not make the claim in the first place.
            //
            // `SkipReason::Other`, and this exact wording, match
            // `UrlIngestor`'s `UrlOutcome::Empty` arm so both paths land in
            // `docs_skipped` rather than `unsupported_format_count` — the
            // format WAS supported. `resources_skipped` (not `errors`) keeps
            // `run_source_ingestion`'s `errors == skip_error_count`
            // cross-check satisfied: nothing failed here.
            if blocks.is_empty() {
                callback
                    .on_skipped(
                        &file.uri,
                        SkipReason::Other("extraction produced no content".to_string()),
                    )
                    .await;
                result.resources_skipped += 1;
                continue;
            }

            let hash = compute_blocks_hash(&blocks);
            let res_id = resource_id(file.uri.as_str(), &hash);

            // Title merge: extraction-level title fills `metadata.title` only
            // when the parser left it `None`. `Resource.title` mirrors the
            // merged metadata title (not `parsed.title` directly), so both
            // fields always agree on which title won.
            let mut dc = parsed.metadata.clone();
            if dc.title.is_none() {
                dc.title = parsed.title.clone();
            }
            let title = dc.title.clone();

            let resource = Resource {
                id: res_id,
                store_id: source.store_id.clone(),
                source_id: source.source_id.clone(),
                ingestor_kind: IngestorKind::File,
                resource_kind: ResourceKind::Document,
                uri: file.uri.clone(),
                external_id: None,
                external_etag: None,
                content_hash: hash,
                title,
                mime,
                metadata: Metadata::Document(DocumentMetadata {
                    dublin_core: dc,
                    ..Default::default()
                }),
                // `added_at` is the ingestion clock — when *we* observed this
                // file — never the file's own mtime; `modified_at` is the
                // mtime-derived value read above.
                added_at: now_rfc3339(),
                modified_at: mtime_rfc3339,
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

/// Compute the `Probe::path_hint` for a filesystem path, tolerating non-UTF-8
/// components anywhere in the path (not just the filename).
///
/// `Path::to_str()` returns `None` as soon as *any* component fails to
/// decode as UTF-8, which would otherwise blind extension-gated parsers on a
/// perfectly valid file (e.g. `notes.md`) simply because it lives under a
/// non-UTF-8-named ancestor directory. This is only used for
/// extension/mime-sniffing hints, never persisted, so a lossy fallback
/// (`to_string_lossy`, replacing invalid sequences with U+FFFD) is safe.
fn path_hint_lossy(path: &std::path::Path) -> String {
    match path.to_str() {
        Some(s) => s.to_string(),
        None => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests;
