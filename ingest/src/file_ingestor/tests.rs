use super::*;
use crate::support::test_doubles::RecordingCallback;
use localdb_core::ingestion::FetchMetadata;
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
            page_starts: Vec::new(),
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
                page_starts: Vec::new(),
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
            page_starts: Vec::new(),
        }))
    }
}

/// Emits a fixed 3-page Markdown document with `page_starts`, regardless
/// of input — exercises the FileIngestor → `markdown_to_blocks_with_pages`
/// page-stamping path (#103) hermetically.
struct PagedParser;
impl Parser for PagedParser {
    fn id(&self) -> &'static str {
        "paged"
    }
    fn parse(&self, _probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
        // Headings break the run into separate blocks so each page's
        // content is its own block (a flat run would fold into one block
        // on page 1 — the coarse-Text packing rule, #158).
        let markdown = "# One\n\nAlpha body on page one.\n\n# Two\n\nBravo body on page two.\n\n# Three\n\nCharlie body on page three.\n"
            .to_string();
        let p2 = markdown.find("# Two").unwrap();
        let p3 = markdown.find("# Three").unwrap();
        Ok(Some(ParsedDocument {
            markdown,
            title: None,
            metadata: localdb_core::metadata::DublinCoreMetadata::default(),
            page_starts: vec![(0, 1), (p2, 2), (p3, 3)],
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
        document_validators: FetchMetadata::default(),
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

/// Issue #187 review finding 2: `self.parser.parse(&probe)` is guarded
/// with `localdb_core::run_blocking` (`core::blocking`) because it may
/// now run under the daemon's shared multi-thread tokio runtime.
/// `run_blocking` takes its `block_in_place` branch only on a
/// multi-thread runtime — the default `#[tokio::test]` used by every
/// other test in this module is current-thread, so it never exercises
/// that branch. This test forces `flavor = "multi_thread"` so a real
/// end-to-end file ingestion actually drives the `block_in_place` path
/// (not just `core::blocking`'s own unit tests in isolation), proving
/// the call site doesn't panic there.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_on_multi_thread_runtime_exercises_block_in_place_guard() {
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
}

/// Codex review finding F6: `enumerate_path_source` (the directory walk)
/// and the per-file `std::fs::read` + `metadata()` hop are now guarded
/// with `localdb_core::run_blocking`, for the same reason as the
/// `parser.parse` call covered by
/// `discovery_on_multi_thread_runtime_exercises_block_in_place_guard`
/// above: this ingestor may run under the daemon's shared multi-thread
/// tokio runtime, and `run_blocking` only takes its `block_in_place`
/// branch there — the default `#[tokio::test]` current-thread runtime
/// never exercises it. This test forces `flavor = "multi_thread"` and
/// includes an unreadable file so the walk, the successful-read hop, and
/// the failed-read hop (which reports `SkipReason::Error`, not a panic)
/// all execute inside `block_in_place` at least once. It does not (and
/// cannot, per the task's stated limitation) prove worker-starvation is
/// avoided — only that the wrapped paths still behave correctly on a
/// multi-thread runtime.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn file_walk_and_read_on_multi_thread_runtime_exercise_block_in_place_guard() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.md"), "# A\n\nContent A.").unwrap();
    std::fs::write(dir.path().join("b.md"), "# B\n\nContent B.").unwrap();
    let unreadable = dir.path().join("unreadable.md");
    std::fs::write(&unreadable, "# C\n\nContent C.").unwrap();
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

    let ingestor = FileIngestor::new(Box::new(AllParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    // Restore permissions so tempdir cleanup can remove the file.
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(cb.discovered, vec![3], "the walk found all three files");
    assert_eq!(
        result.resources_produced, 2,
        "the two readable files are still indexed"
    );
    assert_eq!(
        result.errors, 1,
        "the unreadable file is still reported as a read error"
    );
}

/// #103 end-to-end: a parser emitting `page_starts` produces a resource
/// whose blocks carry the correct per-page `location.page`.
#[tokio::test]
async fn page_starts_flow_into_resource_block_locations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("doc.pdf"), b"ignored by PagedParser").unwrap();

    let ingestor = FileIngestor::new(Box::new(PagedParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    let res = &cb.resources[0];
    let page_of = |needle: &str| {
        res.blocks
            .iter()
            .find(|b| b.text.contains(needle))
            .and_then(|b| b.location.as_ref())
            .and_then(|l| l.page)
    };
    assert_eq!(page_of("Alpha"), Some(1));
    assert_eq!(page_of("Bravo"), Some(2));
    assert_eq!(page_of("Charlie"), Some(3));
}

/// #103 end-to-end through the real `extract` parser chain on the vendored
/// synthetic 3-page PDF fixture: every block resolves to a page, and the
/// distinctive per-page text lands on the expected page.
#[tokio::test]
async fn real_pdf_fixture_stamps_block_pages() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../extract/tests/fixtures/multipage.pdf");
    let bytes = std::fs::read(&fixture).expect("multipage.pdf fixture must exist");

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("multipage.pdf"), &bytes).unwrap();

    let chain = extract::build_chain(&extract::default_parser_ids()).unwrap();
    let ingestor = FileIngestor::new(Box::new(chain));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1, "the PDF should produce one resource");
    let res = &cb.resources[0];
    assert!(!res.blocks.is_empty());
    // Every block carries a page (the whole doc is paginated).
    assert!(
        res.blocks
            .iter()
            .all(|b| b.location.as_ref().and_then(|l| l.page).is_some()),
        "every block of a PDF resource must carry a page"
    );
    let page_of = |needle: &str| {
        res.blocks
            .iter()
            .find(|b| b.text.contains(needle))
            .and_then(|b| b.location.as_ref())
            .and_then(|l| l.page)
    };
    assert_eq!(page_of("quick brown fox"), Some(1));
    assert_eq!(page_of("Sphinx of black quartz"), Some(2));
    assert_eq!(page_of("Pack my box"), Some(3));
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

/// A non-UTF-8 path component must not blank out the whole path hint.
/// `Path::to_str()` returns `None` when *any* component of the path is
/// not valid UTF-8 — including ancestor directories, not just the
/// filename itself — which would otherwise blind extension-gated parsers
/// (they read `Probe::path_hint` for the extension) on an otherwise
/// perfectly supported file. `path_hint_lossy` must fall back to
/// `to_string_lossy` and still expose a usable extension.
///
/// Constructed directly via `OsStrExt` (no real filesystem I/O): on
/// macOS/APFS the kernel itself rejects invalid UTF-8 byte sequences in
/// filenames, so a non-UTF-8 directory can't actually be created there
/// (see `non_utf8_directory_full_ingest_is_still_parsed` below for the
/// filesystem-level exercise, gated to platforms that allow it).
#[cfg(unix)]
#[test]
fn path_hint_lossy_preserves_extension_despite_non_utf8_component() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // 0xFF is not valid UTF-8 in any position, so this directory name
    // cannot round-trip through `OsStr::to_str`, and neither can any
    // path built on top of it.
    let bad_dir = OsStr::from_bytes(b"bad\xFFdir");
    let path = std::path::Path::new("/root").join(bad_dir).join("notes.md");
    assert!(
        path.to_str().is_none(),
        "test setup invariant: the constructed path must not be valid UTF-8"
    );

    let hint = path_hint_lossy(&path);
    let probe = Probe::new(b"# Notes\n\nBody.", Some(hint.as_str()), None);
    assert!(
        MdOnlyParser.parse(&probe).unwrap().is_some(),
        "extension-gated parser should still recognize notes.md via the lossy hint"
    );
}

/// Filesystem-level companion to the test above: a real non-UTF-8
/// directory name, ingested end-to-end, must not cause `notes.md` to be
/// skipped as unsupported. Not run on macOS: APFS/HFS+ reject invalid
/// UTF-8 byte sequences in filenames at the OS level, so this specific
/// repro can't be constructed there (the underlying bug is exercised by
/// `path_hint_lossy_preserves_extension_despite_non_utf8_component`
/// instead, which needs no filesystem support).
#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test]
async fn non_utf8_directory_full_ingest_is_still_parsed() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let bad_name = OsStr::from_bytes(b"bad\xFFdir");
    let subdir = dir.path().join(bad_name);
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(subdir.join("notes.md"), "# Notes\n\nBody.").unwrap();

    let ingestor = FileIngestor::new(Box::new(MdOnlyParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(
        result.resources_produced, 1,
        "notes.md under a non-UTF-8 directory should still be parsed \
         (extension hint preserved), not skipped as unsupported: {:?}",
        cb.skipped
    );
    assert_eq!(result.resources_skipped, 0);
    assert_eq!(cb.resources.len(), 1);
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

/// #185 (defense in depth): a file whose extraction yields zero blocks
/// must be reported via `on_skipped`, never handed to `on_resource` as an
/// empty `Resource`. The sink refuses empty replacements too, but that is
/// a backstop — an ingestor that yields a contentless resource is making a
/// claim ("here is this document's content") it cannot support.
///
/// It must land in `resources_skipped`, not `errors`: nothing failed, so
/// bumping `errors` without a matching `SkipReason::Error` would trip
/// `run_source_ingestion`'s `debug_assert_eq!` cross-check.
#[tokio::test]
async fn zero_block_extraction_is_skipped_not_yielded_as_empty_resource() {
    /// Parses successfully but returns empty Markdown — a whitespace-only
    /// file, a PDF of scanned images with no text layer, an HTML page
    /// whose body is all script tags.
    struct EmptyOutputParser;
    impl Parser for EmptyOutputParser {
        fn id(&self) -> &'static str {
            "empty-output"
        }
        fn parse(&self, _probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
            Ok(Some(ParsedDocument {
                markdown: String::new(),
                title: None,
                metadata: localdb_core::metadata::DublinCoreMetadata::default(),
                page_starts: Vec::new(),
            }))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("blank.md"), "   \n\n  \n").unwrap();

    let ingestor = FileIngestor::new(Box::new(EmptyOutputParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert!(
        cb.resources.is_empty(),
        "a zero-block extraction must never reach on_resource"
    );
    assert_eq!(result.resources_produced, 0);
    assert_eq!(result.resources_skipped, 1);
    assert_eq!(
        result.errors, 0,
        "an empty extraction is a skip, not an error"
    );
    assert_eq!(cb.skipped.len(), 1);
    assert!(cb.skipped[0].0.ends_with("blank.md"));
    // Same reason and wording as `UrlIngestor`'s `UrlOutcome::Empty` arm,
    // so both paths land in `docs_skipped` rather than
    // `unsupported_format_count`.
    assert_eq!(
        cb.skipped[0].1,
        SkipReason::Other("extraction produced no content".to_string())
    );
}

/// The complement to the test above: enumeration over a root that does not
/// exist reports `Enumeration::Incomplete`, which is what suppresses the
/// delete-sweep (#156). Zero resources alone can't carry that signal —
/// an empty-but-present directory produces the same zero.
#[tokio::test]
async fn unreachable_root_reports_incomplete_enumeration() {
    let ingestor = FileIngestor::new(Box::new(AllParser));
    let source = source_with_root("/nonexistent_path_12345");
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert!(
        matches!(&result.enumeration, Enumeration::Incomplete { reason }
                 if reason.contains("/nonexistent_path_12345")),
        "an unreachable root must be reported as an incomplete enumeration \
         naming the root, got: {:?}",
        result.enumeration
    );
    assert_eq!(result.resources_produced, 0);
}

/// ...and an empty-but-present root is `Complete`: the sweep must still
/// run for a source whose files were genuinely all deleted.
#[tokio::test]
async fn empty_but_present_root_reports_complete_enumeration() {
    let dir = tempfile::tempdir().unwrap();
    let ingestor = FileIngestor::new(Box::new(AllParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    let result = ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(result.enumeration, Enumeration::Complete);
    assert_eq!(cb.discovered, vec![0]);
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
                page_starts: Vec::new(),
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
                page_starts: Vec::new(),
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

    // `format_secs_rfc3339` has no cfg(test) fixed-string shortcut (its real
    // formatting logic is exercised directly by
    // `core::ingestion::format_secs_rfc3339_tests`), so compute the expected
    // value from the file's actual mtime via the same shared `core` helper
    // the production code path uses, rather than asserting a hardcoded
    // string that would be flaky against the real filesystem clock.
    let expected_secs = std::fs::metadata(&path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expected = localdb_core::ingestion::format_secs_rfc3339(expected_secs);

    let ingestor = FileIngestor::new(Box::new(AllParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    // `modified_at` is derived from the file's mtime; `added_at` is the
    // ingestion clock, not the mtime — see `file_ingestor_added_at_is_now_not_mtime`
    // and `file_ingestor_modified_at_is_mtime_derived` for that split.
    assert_eq!(cb.resources[0].modified_at, Some(expected.clone()));
    assert!(
        expected.ends_with('Z') && expected.contains('T'),
        "expected an RFC 3339 timestamp, got: {expected}"
    );
}

/// `added_at` records when *we* observed the file (the ingestion clock),
/// never the file's own mtime (#190) — otherwise a file
/// with an old mtime that's only being indexed for the first time today
/// would misreport when it entered the store.
#[tokio::test]
async fn file_ingestor_added_at_is_now_not_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.md");
    std::fs::write(&path, "# X\n\nY.").unwrap();

    // Push mtime far into the past so `added_at` (ingestion time) and
    // `modified_at` (mtime) can't coincide by accident.
    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let file = std::fs::File::open(&path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();

    let before = std::time::SystemTime::now();
    let ingestor = FileIngestor::new(Box::new(AllParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();
    let after = std::time::SystemTime::now();

    assert_eq!(cb.resources.len(), 1);
    let added_at = &cb.resources[0].added_at;

    // RFC 3339 strings in this format are lexicographically ordered, so a
    // plain string comparison against the bracketing wall-clock reads is
    // enough to prove `added_at` came from "now" and not from the (far
    // older) mtime set above.
    let lower = localdb_core::ingestion::format_secs_rfc3339(
        before
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    let upper = localdb_core::ingestion::format_secs_rfc3339(
        after
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 1,
    );
    assert!(
        added_at.as_str() >= lower.as_str() && added_at.as_str() <= upper.as_str(),
        "added_at ({added_at}) must be the ingestion clock (between {lower} and {upper}), \
         not the file's mtime"
    );
}

/// Complement to the test above: `modified_at` still tracks the file's
/// mtime, unaffected by this split.
#[tokio::test]
async fn file_ingestor_modified_at_is_mtime_derived() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.md");
    std::fs::write(&path, "# X\n\nY.").unwrap();

    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let file = std::fs::File::open(&path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();
    let expected = localdb_core::ingestion::format_secs_rfc3339(1_000_000_000);

    let ingestor = FileIngestor::new(Box::new(AllParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    assert_eq!(cb.resources[0].modified_at, Some(expected));
}

/// For a file whose mtime is far in the past, `added_at` (ingestion time)
/// and `modified_at` (mtime) must diverge — pinning that the two are no
/// longer the same clock read for file sources.
#[tokio::test]
async fn file_ingestor_added_at_and_modified_at_differ_for_old_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.md");
    std::fs::write(&path, "# X\n\nY.").unwrap();

    let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let file = std::fs::File::open(&path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(old))
        .unwrap();

    let ingestor = FileIngestor::new(Box::new(AllParser));
    let source = source_with_root(dir.path().to_str().unwrap());
    let mut cb = RecordingCallback::default();
    ingestor.ingest(&source, &mut cb).await.unwrap();

    assert_eq!(cb.resources.len(), 1);
    assert_ne!(
        Some(cb.resources[0].added_at.clone()),
        cb.resources[0].modified_at
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
