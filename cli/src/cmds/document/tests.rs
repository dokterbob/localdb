use super::*;
use localdb_core::metadata::DocumentMetadata;
use localdb_core::{DocumentDetail, DublinCoreMetadata};

fn test_document_info(id: &str, uri: &str, title: Option<&str>) -> DocumentInfo {
    DocumentInfo {
        store_id: "store-1".to_string(),
        id: id.to_string(),
        source_id: "source-1".to_string(),
        ingestor_kind: "file".to_string(),
        uri: uri.to_string(),
        title: title.map(str::to_string),
        mime: Some("text/markdown".to_string()),
        content_hash: "abc123".to_string(),
        fetched_at: "2026-01-01T00:00:00Z".to_string(),
        origin_store: "store-1".to_string(),
        policy_version: "v1".to_string(),
        metadata: Metadata::default(),
    }
}

// --- document list: item conversion + rendering ---

#[test]
fn document_info_to_list_item_copies_fields() {
    let info = test_document_info("doc-1", "/tmp/notes.md", Some("Notes"));
    let item = document_info_to_list_item(&info, "mystore");
    assert_eq!(item.id, "doc-1");
    assert_eq!(item.uri, "/tmp/notes.md");
    assert_eq!(item.title.as_deref(), Some("Notes"));
    assert_eq!(item.store_id, "store-1");
    assert_eq!(item.store_name, "mystore");
    assert_eq!(item.source_id, "source-1");
    assert_eq!(item.content_hash, "abc123");
    assert_eq!(item.fetched_at, "2026-01-01T00:00:00Z");
}

#[test]
fn daemon_item_to_document_list_item_reads_fields_defensively() {
    let raw = json!({
        "id": "doc-2",
        "uri": "https://example.com/page",
        "title": "A Page",
        "store_id": "store-2",
        "source_id": "source-2",
        "content_hash": "def456",
        "fetched_at": "2026-02-02T00:00:00Z",
    });
    let item = daemon_item_to_document_list_item(&raw, "otherstore");
    assert_eq!(item.id, "doc-2");
    assert_eq!(item.uri, "https://example.com/page");
    assert_eq!(item.title.as_deref(), Some("A Page"));
    assert_eq!(item.store_id, "store-2");
    assert_eq!(item.store_name, "otherstore");
    assert_eq!(item.source_id, "source-2");
    assert_eq!(item.content_hash, "def456");
    assert_eq!(item.fetched_at, "2026-02-02T00:00:00Z");
}

#[test]
fn daemon_item_to_document_list_item_tolerates_missing_optional_title() {
    let raw = json!({
        "id": "doc-3",
        "uri": "/tmp/no-title.md",
        "store_id": "store-1",
        "source_id": "source-1",
        "content_hash": "xyz",
        "fetched_at": "2026-01-01T00:00:00Z",
    });
    let item = daemon_item_to_document_list_item(&raw, "s");
    assert_eq!(item.title, None);
}

#[test]
fn document_list_item_human_line_includes_title_when_present() {
    let info = test_document_info("doc-1", "/tmp/notes.md", Some("Notes"));
    let item = document_info_to_list_item(&info, "s");
    let line = document_list_item_human_line(&item, false, 0);
    assert_eq!(line, "doc-1 /tmp/notes.md (Notes)");
}

#[test]
fn document_list_item_human_line_omits_parens_without_title() {
    let info = test_document_info("doc-1", "/tmp/notes.md", None);
    let item = document_info_to_list_item(&info, "s");
    let line = document_list_item_human_line(&item, false, 0);
    assert_eq!(line, "doc-1 /tmp/notes.md");
}

#[test]
fn document_list_item_human_line_multi_store_prefixes_padded_name() {
    let info = test_document_info("doc-1", "/tmp/notes.md", None);
    let width = store_column_width(["books", "default"].into_iter());
    assert_eq!(width, 9); // "default" (7) + 2
    let item = document_info_to_list_item(&info, "books");
    let line = document_list_item_human_line(&item, true, width);
    assert_eq!(line, "books    doc-1 /tmp/notes.md");
}

#[test]
fn document_list_item_json_shape() {
    let info = test_document_info("doc-1", "/tmp/notes.md", Some("Notes"));
    let item = document_info_to_list_item(&info, "mystore");
    let v = document_list_item_json(&item);
    assert_eq!(v["id"], "doc-1");
    assert_eq!(v["uri"], "/tmp/notes.md");
    assert_eq!(v["title"], "Notes");
    assert_eq!(v["store"]["name"], "mystore");
    assert_eq!(v["store_id"], "store-1");
    assert_eq!(v["source_id"], "source-1");
    assert_eq!(v["content_hash"], "abc123");
    assert_eq!(v["fetched_at"], "2026-01-01T00:00:00Z");
}

// --- document get: DocumentGetResult construction ---

fn sample_metadata() -> Metadata {
    Metadata::Document(DocumentMetadata {
        dublin_core: DublinCoreMetadata {
            title: Some("Dublin Title".to_string()),
            creator: vec!["Alice".to_string(), "Bob".to_string()],
            language: Some("en".to_string()),
            ..Default::default()
        },
        page_count: Some(3),
        word_count: Some(42),
    })
}

#[test]
fn document_get_result_from_detail_populates_text_from_detail() {
    let info = test_document_info("doc-1", "/tmp/notes.md", Some("Notes"));
    let detail = DocumentDetail {
        info,
        text: Some("full document text".to_string()),
    };
    let result = DocumentGetResult::from_detail(detail);
    assert_eq!(result.id, "doc-1");
    assert_eq!(result.text, "full document text");
}

#[test]
fn document_get_result_from_detail_defaults_missing_text_to_empty_string() {
    let info = test_document_info("doc-1", "/tmp/notes.md", None);
    let detail = DocumentDetail { info, text: None };
    let result = DocumentGetResult::from_detail(detail);
    assert_eq!(result.text, "");
}

#[test]
fn document_get_result_from_daemon_json_parses_full_document_record_shape() {
    let raw = json!({
        "id": "doc-1",
        "uri": "/tmp/notes.md",
        "title": "Notes",
        "store_id": "store-1",
        "source_id": "source-1",
        "content_hash": "abc123",
        "fetched_at": "2026-01-01T00:00:00Z",
        "normalized_text": "full document text",
        "metadata": serde_json::to_value(sample_metadata()).unwrap(),
    });
    let result = document_get_result_from_daemon_json(&raw).unwrap();
    assert_eq!(result.id, "doc-1");
    assert_eq!(result.uri, "/tmp/notes.md");
    assert_eq!(result.title.as_deref(), Some("Notes"));
    assert_eq!(result.store_id, "store-1");
    assert_eq!(result.source_id, "source-1");
    assert_eq!(result.content_hash, "abc123");
    assert_eq!(result.fetched_at, "2026-01-01T00:00:00Z");
    assert_eq!(result.text, "full document text");
    assert_eq!(
        result.metadata.dublin_core().title.as_deref(),
        Some("Dublin Title")
    );
}

#[test]
fn document_get_result_from_daemon_json_rejects_missing_required_field() {
    let raw = json!({
        "uri": "/tmp/notes.md",
        "store_id": "store-1",
        "source_id": "source-1",
        "content_hash": "abc123",
        "fetched_at": "2026-01-01T00:00:00Z",
        "normalized_text": "text",
        "metadata": serde_json::to_value(sample_metadata()).unwrap(),
    });
    let err = document_get_result_from_daemon_json(&raw).unwrap_err();
    assert!(matches!(err, Error::Internal { .. }));
}

#[test]
fn document_get_result_from_daemon_json_rejects_malformed_metadata() {
    let raw = json!({
        "id": "doc-1",
        "uri": "/tmp/notes.md",
        "store_id": "store-1",
        "source_id": "source-1",
        "content_hash": "abc123",
        "fetched_at": "2026-01-01T00:00:00Z",
        "normalized_text": "text",
        "metadata": { "kind": "not-a-real-kind" },
    });
    let err = document_get_result_from_daemon_json(&raw).unwrap_err();
    assert!(matches!(err, Error::Internal { .. }));
}

// --- document get: rendering ---

fn sample_get_result(text: &str) -> DocumentGetResult {
    DocumentGetResult {
        id: "doc-1".to_string(),
        uri: "/tmp/notes.md".to_string(),
        title: Some("Notes".to_string()),
        store_id: "store-1".to_string(),
        source_id: "source-1".to_string(),
        content_hash: "abc123".to_string(),
        fetched_at: "2026-01-01T00:00:00Z".to_string(),
        metadata: sample_metadata(),
        text: text.to_string(),
    }
}

#[test]
fn document_get_human_lines_without_text_omits_body() {
    let doc = sample_get_result("the full body");
    let lines = document_get_human_lines(&doc, false);
    assert_eq!(lines[0], "id: doc-1");
    assert_eq!(lines[1], "uri: /tmp/notes.md");
    assert!(lines.contains(&"title: Notes".to_string()));
    assert!(lines.contains(&"store_id: store-1".to_string()));
    assert!(lines.contains(&"source_id: source-1".to_string()));
    assert!(lines.contains(&"content_hash: abc123".to_string()));
    assert!(lines.contains(&"fetched_at: 2026-01-01T00:00:00Z".to_string()));
    assert!(lines.contains(&"dc.creator: Alice, Bob".to_string()));
    assert!(lines.contains(&"dc.language: en".to_string()));
    assert!(
        !lines.iter().any(|l| l == "the full body"),
        "text must not appear without --text: {lines:?}"
    );
}

#[test]
fn document_get_human_lines_with_text_appends_body_after_blank_line() {
    let doc = sample_get_result("the full body");
    let lines = document_get_human_lines(&doc, true);
    assert_eq!(lines[lines.len() - 2], "");
    assert_eq!(lines[lines.len() - 1], "the full body");
}

#[test]
fn document_get_human_lines_skips_absent_dublin_core_fields() {
    let mut doc = sample_get_result("body");
    doc.metadata = Metadata::default();
    let lines = document_get_human_lines(&doc, false);
    assert!(!lines.iter().any(|l| l.starts_with("dc.creator")));
    assert!(!lines.iter().any(|l| l.starts_with("dc.language")));
}

#[test]
fn document_get_result_json_always_includes_text_regardless_of_flag() {
    // `--text` only governs the human renderer; the JSON shape always
    // carries the fetched text (see `DocumentGetResult`'s doc comment).
    let doc = sample_get_result("the full body");
    let v = document_get_result_json(&doc);
    assert_eq!(v["id"], "doc-1");
    assert_eq!(v["uri"], "/tmp/notes.md");
    assert_eq!(v["title"], "Notes");
    assert_eq!(v["store_id"], "store-1");
    assert_eq!(v["source_id"], "source-1");
    assert_eq!(v["content_hash"], "abc123");
    assert_eq!(v["fetched_at"], "2026-01-01T00:00:00Z");
    assert_eq!(v["text"], "the full body");
    assert_eq!(v["metadata"]["kind"], "document");
}

// --- render_document_list: empty-scope messaging ---
// (`render_document_get`/`render_document_list` themselves only ever
// `println!`, so their observable behavior beyond the pure helpers above is
// covered at the CLI-integration level in `localdb/tests/cli_document.rs`.)
