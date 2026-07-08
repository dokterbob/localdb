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
        let files = enumerate_path_source(root, &include, &exclude)?;

        // Signal `Discovered { total }` right after enumeration and before
        // processing the first file.
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

            // mtime -> fetched_at/added_at/modified_at, formatted as RFC 3339
            // (falls back to "now" if the filesystem doesn't report a
            // modified time).
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

            // Two distinct mime computations:
            //  - `detect_mime` (extension-based) is what gets stamped onto
            //    the stored document/chunk metadata.
            //  - `extract::sniff_mime` (magic bytes + extension) feeds into
            //    `Probe.sniffed_mime` before calling the parser chain.
            let mime = detect_mime(&file.path);
            let sniffed = sniff_mime(&bytes, filename.as_deref());
            let probe = Probe::new(&bytes, file.path.to_str(), sniffed.as_deref());

            // Panic-tolerant parsing: a panicking parser must not crash the
            // whole walk. `catch_panic` wraps extraction and the panic is
            // surfaced via `on_skipped` + `SkipReason::Error` (a panic IS an
            // error, matching the old pipeline's behavior of folding panics
            // into the error count, C8) rather than the benign-skip counter.
            let parsed = match catch_panic(std::panic::AssertUnwindSafe(|| {
                self.parser.parse(&probe)
            })) {
                Err(panic_msg) => {
                    tracing::warn!(uri = %file.uri, "FileIngestor: parser panicked: {}", panic_msg);
                    callback
                        .on_skipped(&file.uri, SkipReason::Error(panic_msg))
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
                    tracing::warn!(uri = %file.uri, "FileIngestor: parser error: {}", e);
                    // Same aliveness rule as the read-error path above;
                    // SkipReason::Error so it's counted as an error (C8).
                    callback
                        .on_skipped(&file.uri, SkipReason::Error(format!("parser error: {e}")))
                        .await;
                    result.errors += 1;
                    continue;
                }
            };

            let blocks = markdown_to_blocks(&parsed.markdown);
            let hash = compute_blocks_hash(&blocks);
            let res_id = resource_id(&file.uri, &hash);

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
        // C8: a parser panic is an error, not a benign skip — it must be
        // counted in `errors`/`SkipReason::Error`, not
        // `resources_skipped`/`SkipReason::Other` (matching the old
        // pipeline, which folded panics into the error count).
        assert_eq!(result.resources_skipped, 0);
        assert_eq!(result.errors, 1);
        assert_eq!(cb.skipped.len(), 1);
        assert!(cb.skipped[0].0.ends_with("b.boom"));
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Error(msg) if msg.contains("simulated parser panic")),
            "expected SkipReason::Error with the panic message, got: {:?}",
            cb.skipped[0].1
        );
    }

    #[tokio::test]
    async fn parser_error_is_reported_as_skip_reason_error() {
        struct FailingParser;
        impl Parser for FailingParser {
            fn id(&self) -> &'static str {
                "failing"
            }
            fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
                if probe.path_hint.is_some_and(|p| p.ends_with(".fail")) {
                    return Err(Error::Internal {
                        message: "simulated parser error".to_string(),
                        correlation_id: "test_parser_error".to_string(),
                    });
                }
                let text = String::from_utf8_lossy(probe.bytes()).to_string();
                Ok(Some(ParsedDocument {
                    markdown: text,
                    title: None,
                    metadata: localdb_core::metadata::DublinCoreMetadata::default(),
                }))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "# A\n\nContent A.").unwrap();
        std::fs::write(dir.path().join("b.fail"), "will error").unwrap();

        let ingestor = FileIngestor::new(Box::new(FailingParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(result.resources_produced, 1, "the good file still indexes");
        assert_eq!(result.errors, 1, "the parser error counts as an error");
        assert_eq!(cb.skipped.len(), 1);
        assert!(cb.skipped[0].0.ends_with("b.fail"));
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Error(msg) if msg.contains("simulated parser error")),
            "parser-error path must report SkipReason::Error so the delete-sweep \
             keeps this still-present file's indexed content alive; got: {:?}",
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
        let path = dir.path().join("doc.md");
        std::fs::write(&path, "# X\n\nY.").unwrap();

        // `format_unix_secs` no longer has a cfg(test) fixed-string shortcut
        // (its real formatting logic is exercised directly by
        // `support::format_unix_secs_tests`), so compute the expected value
        // from the file's actual mtime via the same crate-local helper the
        // production code path uses, rather than asserting a hardcoded
        // string that would be flaky against the real filesystem clock.
        let expected_secs = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expected = crate::support::format_unix_secs(expected_secs);

        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = source_with_root(dir.path().to_str().unwrap());
        let mut cb = RecordingCallback::default();
        ingestor.ingest(&source, &mut cb).await.unwrap();

        assert_eq!(cb.resources.len(), 1);
        assert_eq!(cb.resources[0].added_at, expected);
        assert_eq!(cb.resources[0].modified_at, expected);
        assert!(
            expected.ends_with('Z') && expected.contains('T'),
            "expected an RFC 3339 timestamp, got: {expected}"
        );
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
        assert_eq!(cb.skipped.len(), 1);
        assert!(
            matches!(&cb.skipped[0].1, SkipReason::Error(msg) if msg.contains("read error")),
            "read errors must be reported as SkipReason::Error so the delete-sweep \
             keeps this still-present file's indexed content alive; got: {:?}",
            cb.skipped[0].1
        );
    }
}
