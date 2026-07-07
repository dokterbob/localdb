//! File-system ingestor: scans a directory tree, parses each file, and emits
//! typed [`Resource`]s.
//!
//! Full-parity port of `core::ingestion::run_path_source` onto the
//! [`Ingestor`] trait (issue #117). Behavior is intentionally identical to
//! `run_path_source` wherever the `Ingestor`/`IngestCallback` contract allows
//! it to be expressed; deviations are called out inline.

use extract::sniff_mime;
use localdb_core::block::{IngestorKind, Resource, ResourceKind};
use localdb_core::error::Error;
use localdb_core::ids::resource_id;
use localdb_core::ingestion::{enumerate_path_source, now_rfc3339};
use localdb_core::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor, SkipReason};
use localdb_core::markdown_blocks::{compute_blocks_hash, markdown_to_blocks};
use localdb_core::metadata::{DocumentMetadata, Metadata};
use localdb_core::parser::{Parser, Probe};
use localdb_core::uri::Uri;

use crate::support::{catch_panic, detect_mime, format_unix_secs};

/// File-system ingestor.
///
/// Reads a directory tree from `source.config["root"]`, optionally filtered by
/// `source.config["include"]` (array of glob patterns) and
/// `source.config["exclude"]` (array of glob patterns) — identical config
/// shape and semantics to `run_path_source` (both call the same
/// `core::ingestion::enumerate_path_source`).
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

        // Same enumeration helper `run_path_source` uses, so directory-walk,
        // hidden-file, extension and glob filtering behavior is identical by
        // construction (there's only one implementation).
        let files = enumerate_path_source(root, &include, &exclude)?;

        // Parity: run_path_source signals `Discovered { total }` right after
        // enumeration and before processing the first file.
        callback.on_discovered(files.len()).await;

        let mut result = IngestResult::default();

        for file in &files {
            let bytes = match std::fs::read(&file.path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %file.path.display(), "FileIngestor: failed to read file: {}", e);
                    // Report via on_skipped so the delete-sweep keeps this
                    // still-existing file's indexed content: only URIs never
                    // reported at all get swept, and a transient read error
                    // must not delete good chunks.
                    callback
                        .on_skipped(&file.uri, SkipReason::Other(format!("read error: {e}")))
                        .await;
                    result.errors += 1;
                    continue;
                }
            };

            // mtime -> fetched_at/added_at/modified_at, mirroring
            // run_path_source's RFC 3339 formatting (falls back to "now" if
            // the filesystem doesn't report a modified time).
            let fetched_at = file
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

            let filename = file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string());

            // Two distinct mime computations, mirroring the split that exists
            // in the real pipeline:
            //  - `detect_mime` (extension-based) is what run_path_source
            //    stamps onto the stored document/chunk metadata.
            //  - `extract::sniff_mime` (magic bytes + extension) is what
            //    `ChainExtractor` — the real `DocumentExtractor` run_path_source
            //    is wired to in production — feeds into `Probe.sniffed_mime`
            //    before calling the parser chain.
            let mime = detect_mime(&file.path);
            let sniffed = sniff_mime(&bytes, filename.as_deref());
            let probe = Probe::new(&bytes, file.path.to_str(), sniffed.as_deref());

            // Panic-tolerant parsing: a panicking parser must not crash the
            // whole walk. Mirrors run_path_source's `catch_panic` wrapping of
            // extraction, but surfaces the panic via `on_skipped` +
            // `SkipReason::Other` (this trait's dedicated skip-reason
            // channel) rather than folding it into the generic error count.
            let parsed = match catch_panic(std::panic::AssertUnwindSafe(|| {
                self.parser.parse(&probe)
            })) {
                Err(panic_msg) => {
                    tracing::warn!(uri = %file.uri, "FileIngestor: parser panicked: {}", panic_msg);
                    callback
                        .on_skipped(&file.uri, SkipReason::Other(panic_msg))
                        .await;
                    result.resources_skipped += 1;
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
                    tracing::warn!(uri = %file.uri, "FileIngestor: parser error: {}", e);
                    // Same aliveness rule as the read-error path above.
                    callback
                        .on_skipped(&file.uri, SkipReason::Other(format!("parser error: {e}")))
                        .await;
                    result.errors += 1;
                    continue;
                }
            };

            let blocks = markdown_to_blocks(&parsed.markdown);
            let hash = compute_blocks_hash(&blocks);
            let res_id = resource_id(&file.uri, &hash);

            // Title merge: extraction-level title fills `metadata.title` only
            // when the parser left it `None` — the same rule
            // `index_document` applies. `Resource.title` mirrors the merged
            // metadata title (not `parsed.title` directly), so both fields
            // always agree on which title won.
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
                uri: Uri::parse(&file.uri).ok_or_else(|| Error::Internal {
                    message: format!("FileIngestor: invalid URI '{}'", file.uri),
                    correlation_id: "file_ingestor_uri".to_string(),
                })?,
                external_id: None,
                external_etag: None,
                content_hash: hash,
                title,
                mime,
                metadata: Metadata::Document(DocumentMetadata {
                    dublin_core: dc,
                    ..Default::default()
                }),
                added_at: fetched_at.clone(),
                modified_at: fetched_at,
                thread_id: None,
                channel: None,
                participants: vec![],
                origin_store: source.store_id.clone(),
                // Parity fix vs. the pre-existing `core::ingestors::FileIngestor`,
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

    /// A minimal parser for tests: accepts everything, returns the bytes as Markdown.
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

    /// Parses `.md` files, declines everything else — used to exercise the
    /// unsupported-format skip path alongside successful parses in the same run.
    struct MdOnlyParser;
    impl Parser for MdOnlyParser {
        fn id(&self) -> &'static str {
            "md-only"
        }
        fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
            if probe.path_hint.is_some_and(|p| p.ends_with(".md")) {
                let text = String::from_utf8_lossy(probe.bytes()).to_string();
                Ok(Some(ParsedDocument {
                    markdown: text,
                    title: None,
                    metadata: localdb_core::metadata::DublinCoreMetadata::default(),
                }))
            } else {
                Ok(None)
            }
        }
    }

    /// Panics on files whose path hint ends in `.boom`, parses everything else.
    struct PanickingParser;
    impl Parser for PanickingParser {
        fn id(&self) -> &'static str {
            "panicking"
        }
        fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
            if probe.path_hint.is_some_and(|p| p.ends_with(".boom")) {
                panic!("simulated parser panic");
            }
            let text = String::from_utf8_lossy(probe.bytes()).to_string();
            Ok(Some(ParsedDocument {
                markdown: text,
                title: None,
                metadata: localdb_core::metadata::DublinCoreMetadata::default(),
            }))
        }
    }

    fn source_with_root(root: &str) -> IngestSource {
        IngestSource {
            policy_version: "policy-xyz".to_string(),
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::File,
            config: serde_json::json!({"root": root}),
        }
    }

    #[tokio::test]
    async fn missing_root_errors() {
        let ingestor = FileIngestor::new(Box::new(ChainParser::new("chain", vec![])));
        let source = IngestSource {
            config: serde_json::json!({}),
            ..source_with_root("/unused")
        };
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await;
        assert!(result.is_err(), "missing root should error");
    }

    #[tokio::test]
    async fn nonexistent_root_produces_no_resources_but_still_reports_discovered() {
        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = source_with_root("/nonexistent_path_12345");
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();
        assert_eq!(result.resources_produced, 0);
        assert!(cb.resources.is_empty());
        assert_eq!(cb.discovered, vec![0]);
    }

    #[tokio::test]
    async fn discovery_count_and_resources_match_directory_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# A\n\nContent A.").unwrap();
        std::fs::write(dir.path().join("b.md"), "# B\n\nContent B.").unwrap();

        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(cb.discovered, vec![2]);
        assert_eq!(result.resources_produced, 2);
        assert_eq!(cb.resources.len(), 2);
        for res in &cb.resources {
            assert!(!res.blocks.is_empty(), "resource should have blocks");
            assert_eq!(res.store_id, "store-1");
            assert_eq!(res.source_id, "src-1");
            assert_eq!(res.ingestor_kind, IngestorKind::File);
            // Parity fix: policy_version comes from the source, not "v1".
            assert_eq!(res.policy_version, "policy-xyz");
        }
    }

    #[tokio::test]
    async fn unsupported_format_is_skipped_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# A\n\nContent A.").unwrap();
        std::fs::write(dir.path().join("b.bin"), b"\xFF\xFE\x00\x01").unwrap();

        let ingestor = FileIngestor::new(Box::new(MdOnlyParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_produced, 1);
        assert_eq!(result.resources_skipped, 1);
        assert_eq!(cb.skipped.len(), 1);
        assert!(cb.skipped[0].0.ends_with("b.bin"));
        assert_eq!(cb.skipped[0].1, SkipReason::Unsupported);
    }

    #[tokio::test]
    async fn panicking_parser_is_skipped_not_crashed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# A\n\nContent A.").unwrap();
        std::fs::write(dir.path().join("b.boom"), "trigger panic").unwrap();

        let ingestor = FileIngestor::new(Box::new(PanickingParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        // The whole run must complete (no propagated panic), and the good
        // file must still be processed even though it's enumerated after the
        // panicking one in a directory listing sorted by path ("a" < "b").
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(
            result.resources_produced, 1,
            "the non-panicking file is still indexed"
        );
        assert_eq!(result.resources_skipped, 1);
        assert_eq!(cb.skipped.len(), 1);
        assert!(cb.skipped[0].0.ends_with("b.boom"));
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Other(msg) if msg.contains("simulated parser panic")),
            "expected SkipReason::Other with the panic message, got: {:?}",
            cb.skipped[0].1
        );
    }

    #[tokio::test]
    async fn title_merge_fills_metadata_title_only_when_absent() {
        struct TitledParser;
        impl Parser for TitledParser {
            fn id(&self) -> &'static str {
                "titled"
            }
            fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
                let text = String::from_utf8_lossy(probe.bytes()).to_string();
                // Filename decides which title-merge case this file exercises.
                let metadata_title = if probe.path_hint.is_some_and(|p| p.contains("meta-wins")) {
                    Some("Metadata Title".to_string())
                } else {
                    None
                };
                Ok(Some(ParsedDocument {
                    markdown: text,
                    title: Some("Extraction Title".to_string()),
                    metadata: localdb_core::metadata::DublinCoreMetadata {
                        title: metadata_title,
                        ..Default::default()
                    },
                }))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fills-from-extraction.md"), "# X\n\nY.").unwrap();
        std::fs::write(dir.path().join("meta-wins.md"), "# X\n\nY.").unwrap();

        let ingestor = FileIngestor::new(Box::new(TitledParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(cb.resources.len(), 2);
        for res in &cb.resources {
            if res.uri.to_string().contains("meta-wins") {
                assert_eq!(res.title.as_deref(), Some("Metadata Title"));
            } else {
                assert_eq!(res.title.as_deref(), Some("Extraction Title"));
            }
            // Resource.title always mirrors metadata.dublin_core.title.
            assert_eq!(res.title, res.metadata.dublin_core().title);
        }
    }

    #[tokio::test]
    async fn mime_is_detected_from_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), "# X\n\nY.").unwrap();

        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(cb.resources.len(), 1);
        assert_eq!(cb.resources[0].mime.as_deref(), Some("text/markdown"));
    }

    #[tokio::test]
    async fn mtime_is_formatted_as_rfc3339() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doc.md"), "# X\n\nY.").unwrap();

        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(cb.resources.len(), 1);
        // ingest is compiled with cfg(test) for its own test runs, so the
        // mirrored `format_unix_secs` takes its deterministic-string branch
        // (same trick `core::ingestion` uses for its own tests).
        assert_eq!(cb.resources[0].added_at, "2026-06-10T12:00:00Z");
        assert_eq!(cb.resources[0].modified_at, "2026-06-10T12:00:00Z");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_file_is_counted_as_error_and_walk_continues() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let unreadable = dir.path().join("unreadable.md");
        std::fs::write(&unreadable, "# X\n\nY.").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::write(dir.path().join("stays.md"), "# X\n\nY.").unwrap();

        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        // Restore permissions so tempdir cleanup can remove the file.
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            result.errors, 1,
            "the unreadable file is counted as an error"
        );
        assert_eq!(
            result.resources_produced, 1,
            "the walk continues past the unreadable file"
        );
    }
}
