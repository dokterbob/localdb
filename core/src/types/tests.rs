use super::*;
use crate::ids::{chunk_id, content_hash, new_ulid, resource_id};

fn make_provenance() -> Provenance {
    Provenance {
        origin_store: new_ulid(),
        source_ref: SourceRef {
            id: new_ulid(),
            kind: "path".to_string(),
        },
        fetched_at: "2026-06-10T12:00:00Z".to_string(),
        content_hash: content_hash("test content"),
        share_path: vec![],
    }
}

// --- Provenance tests ---

#[test]
fn provenance_serializes_roundtrip() {
    let p = make_provenance();
    let json = serde_json::to_string(&p).unwrap();
    let p2: Provenance = serde_json::from_str(&json).unwrap();
    assert_eq!(p, p2);
}

#[test]
fn provenance_share_path_defaults_empty() {
    let json = r#"{
        "origin_store": "01HN1Y28MYWN6X5DSKZMNE1T5W",
        "source_ref": {"id": "01HN1Y28MYWN6X5DSKZMNE1T5X", "kind": "path"},
        "fetched_at": "2026-06-10T12:00:00Z",
        "content_hash": "abc123"
    }"#;
    let p: Provenance = serde_json::from_str(json).unwrap();
    assert!(p.share_path.is_empty());
}

// --- Store tests ---

#[test]
fn store_serializes_roundtrip() {
    let store = Store {
        id: new_ulid(),
        name: "test-store".to_string(),
        visibility: StoreVisibility::Private,
        backend: BackendConfig {
            kind: "libsql".to_string(),
            connection: HashMap::new(),
        },
        indexing: IndexingPolicy {
            chunking: ChunkingConfig {
                preset: "prose".to_string(),
                max_chars: Some(1024),
                overlap_chars: Some(128),
            },
            embedding: EmbeddingConfig {
                provider: "local-onnx".to_string(),
                model: "default".to_string(),
            },
        },
    };
    let json = serde_json::to_string(&store).unwrap();
    let store2: Store = serde_json::from_str(&json).unwrap();
    assert_eq!(store, store2);
}

#[test]
fn store_visibility_parse_roundtrips_wire_strings() {
    assert_eq!(
        StoreVisibility::parse("private"),
        Some(StoreVisibility::Private)
    );
    assert_eq!(
        StoreVisibility::parse("shared"),
        Some(StoreVisibility::Shared)
    );
    assert_eq!(StoreVisibility::parse("public"), None);
    assert_eq!(StoreVisibility::parse(""), None);
}

#[test]
fn store_visibility_serializes_lowercase() {
    let v = StoreVisibility::Private;
    let json = serde_json::to_value(&v).unwrap();
    assert_eq!(json, serde_json::json!("private"));

    let v2 = StoreVisibility::Shared;
    let json2 = serde_json::to_value(&v2).unwrap();
    assert_eq!(json2, serde_json::json!("shared"));
}

// --- Source tests ---

#[test]
fn source_serializes_roundtrip() {
    let source = Source {
        id: new_ulid(),
        store_id: new_ulid(),
        kind: SourceKind::Path,
        spec: SourceSpec::Path {
            root: "/home/user/docs".to_string(),
            include: vec!["**/*.md".to_string()],
            exclude: vec![".git/**".to_string()],
        },
        source_preset: "prose".to_string(),
    };
    let json = serde_json::to_string(&source).unwrap();
    let source2: Source = serde_json::from_str(&json).unwrap();
    assert_eq!(source, source2);
}

#[test]
fn url_source_serializes_roundtrip() {
    let source = Source {
        id: new_ulid(),
        store_id: new_ulid(),
        kind: SourceKind::Url,
        spec: SourceSpec::Url {
            url: "https://example.com/docs".to_string(),
            refresh_interval_secs: Some(3600),
        },
        source_preset: "prose".to_string(),
    };
    let json = serde_json::to_string(&source).unwrap();
    let source2: Source = serde_json::from_str(&json).unwrap();
    assert_eq!(source, source2);
}

#[test]
fn feed_source_serializes_roundtrip() {
    let source = Source {
        id: new_ulid(),
        store_id: new_ulid(),
        kind: SourceKind::Feed,
        spec: SourceSpec::Feed {
            url: "https://example.com/feed.xml".to_string(),
            max_entries: Some(50),
            fetch_full_content: false,
            refresh_interval_secs: Some(3600),
        },
        source_preset: "prose".to_string(),
    };
    let json = serde_json::to_string(&source).unwrap();
    let source2: Source = serde_json::from_str(&json).unwrap();
    assert_eq!(source, source2);
}

#[test]
fn feed_source_spec_serializes_with_tag_feed() {
    let spec = SourceSpec::Feed {
        url: "https://example.com/feed.xml".to_string(),
        max_entries: None,
        fetch_full_content: true,
        refresh_interval_secs: None,
    };
    let json = serde_json::to_value(&spec).unwrap();
    assert_eq!(json.get("type").and_then(|v| v.as_str()), Some("feed"));
}

#[test]
fn feed_source_kind_serializes_lowercase() {
    let json = serde_json::to_value(SourceKind::Feed).unwrap();
    assert_eq!(json, serde_json::json!("feed"));
}

#[test]
fn feed_source_spec_omitted_fetch_full_content_defaults_true() {
    let json = r#"{"type": "feed", "url": "https://example.com/feed.xml"}"#;
    let spec: SourceSpec = serde_json::from_str(json).unwrap();
    match spec {
        SourceSpec::Feed {
            fetch_full_content,
            max_entries,
            refresh_interval_secs,
            ..
        } => {
            assert!(
                fetch_full_content,
                "fetch_full_content must default to true"
            );
            assert_eq!(max_entries, None);
            assert_eq!(refresh_interval_secs, None);
        }
        _ => panic!("expected feed spec"),
    }
}

// --- Document tests ---

#[test]
fn document_serializes_roundtrip() {
    let hash = content_hash("some document content");
    let doc = Document {
        id: resource_id("file:///docs/readme.md", &hash),
        source_id: new_ulid(),
        store_id: new_ulid(),
        uri: "file:///docs/readme.md".to_string(),
        title: Some("README".to_string()),
        mime: Some("text/markdown".to_string()),
        lang: Some("en".to_string()),
        content_hash: hash,
        provenance: make_provenance(),
        meta: HashMap::new(),
    };
    let json = serde_json::to_string(&doc).unwrap();
    let doc2: Document = serde_json::from_str(&json).unwrap();
    assert_eq!(doc, doc2);
}

#[test]
fn document_meta_allows_valid_msg_keys() {
    let hash = content_hash("content");
    let mut meta = HashMap::new();
    meta.insert("msg.thread_id".to_string(), serde_json::json!("thread-123"));
    meta.insert(
        "msg.participants".to_string(),
        serde_json::json!(["alice", "bob"]),
    );
    meta.insert(
        "msg.sent_at".to_string(),
        serde_json::json!("2026-06-10T12:00:00Z"),
    );
    meta.insert("msg.in_reply_to".to_string(), serde_json::json!("msg-456"));
    meta.insert("msg.channel".to_string(), serde_json::json!("#general"));

    let doc = Document {
        id: resource_id("imap://acct/folder;uid=1", &hash),
        source_id: new_ulid(),
        store_id: new_ulid(),
        uri: "imap://acct/folder;uid=1".to_string(),
        title: None,
        mime: None,
        lang: None,
        content_hash: hash,
        provenance: make_provenance(),
        meta,
    };
    assert!(
        doc.validate_meta().is_ok(),
        "all reserved msg.* keys should be valid"
    );
}

#[test]
fn document_meta_rejects_unknown_msg_keys() {
    let hash = content_hash("content");
    let mut meta = HashMap::new();
    meta.insert("msg.unknown_key".to_string(), serde_json::json!("value"));

    let doc = Document {
        id: resource_id("file:///test.md", &hash),
        source_id: new_ulid(),
        store_id: new_ulid(),
        uri: "file:///test.md".to_string(),
        title: None,
        mime: None,
        lang: None,
        content_hash: hash,
        provenance: make_provenance(),
        meta,
    };
    assert!(
        doc.validate_meta().is_err(),
        "unknown msg.* key should fail validation"
    );
}

#[test]
fn document_meta_allows_non_msg_keys() {
    let hash = content_hash("content");
    let mut meta = HashMap::new();
    meta.insert("custom.my_key".to_string(), serde_json::json!("value"));
    meta.insert("app_specific".to_string(), serde_json::json!(42));

    let doc = Document {
        id: resource_id("file:///test.md", &hash),
        source_id: new_ulid(),
        store_id: new_ulid(),
        uri: "file:///test.md".to_string(),
        title: None,
        mime: None,
        lang: None,
        content_hash: hash,
        provenance: make_provenance(),
        meta,
    };
    assert!(
        doc.validate_meta().is_ok(),
        "non-msg.* keys should be allowed"
    );
}

#[test]
fn validate_msg_meta_key_accepts_all_reserved_keys() {
    assert!(validate_msg_meta_key("msg.thread_id").is_ok());
    assert!(validate_msg_meta_key("msg.participants").is_ok());
    assert!(validate_msg_meta_key("msg.sent_at").is_ok());
    assert!(validate_msg_meta_key("msg.in_reply_to").is_ok());
    assert!(validate_msg_meta_key("msg.channel").is_ok());
}

#[test]
fn validate_msg_meta_key_rejects_unknown_keys() {
    assert!(validate_msg_meta_key("msg.foo").is_err());
    assert!(validate_msg_meta_key("msg.").is_err());
    assert!(validate_msg_meta_key("msg.THREAD_ID").is_err()); // case sensitive
}

// --- Chunk tests ---

#[test]
fn chunk_serializes_roundtrip() {
    let doc_id = resource_id("file:///docs/api.md", &content_hash("doc content"));
    let text = "This is a chunk of text.";
    let span = Span::new(0, text.len());
    let id = chunk_id(&doc_id, 0, text, 0);

    let chunk = Chunk {
        id,
        resource_id: doc_id,
        store_id: new_ulid(),
        text: text.to_string(),
        span,
        heading_path: vec!["API".to_string(), "Introduction".to_string()],
        policy_version: "abc123def456".to_string(),
        provenance: make_provenance(),
        window_block_seqs: vec![],
    };
    let json = serde_json::to_string(&chunk).unwrap();
    let chunk2: Chunk = serde_json::from_str(&json).unwrap();
    assert_eq!(chunk, chunk2);
}

#[test]
fn chunk_window_block_seqs_defaults_empty_on_missing_field() {
    // Old JSON, written before window_block_seqs existed, must still deserialize.
    let doc_id = resource_id("file:///docs/api.md", &content_hash("doc content"));
    let text = "This is a chunk of text.";
    let json = format!(
        r#"{{
            "id": "{id}",
            "resource_id": "{doc_id}",
            "store_id": "store-1",
            "text": "{text}",
            "span": {{"start": 0, "end": {len}}},
            "policy_version": "v1",
            "provenance": {{
                "origin_store": "store-1",
                "source_ref": {{"id": "src-1", "kind": "path"}},
                "fetched_at": "2026-06-10T12:00:00Z",
                "content_hash": "abc123"
            }}
        }}"#,
        id = chunk_id(&doc_id, 0, text, 0),
        len = text.len(),
    );
    let chunk: Chunk = serde_json::from_str(&json).unwrap();
    assert!(chunk.window_block_seqs.is_empty());
}

// --- IndexJob tests ---

#[test]
fn index_job_serializes_roundtrip() {
    let job = IndexJob {
        id: new_ulid(),
        store_id: new_ulid(),
        scope: IndexJobScope::Store,
        state: IndexJobState::Pending,
        stats: IndexJobStats::default(),
        error: None,
        error_code: None,
        created_at: "2026-06-10T12:00:00Z".to_string(),
        started_at: None,
        completed_at: None,
    };
    let json = serde_json::to_string(&job).unwrap();
    let job2: IndexJob = serde_json::from_str(&json).unwrap();
    assert_eq!(job, job2);
}

#[test]
fn index_job_error_code_defaults_to_none_when_absent_from_json() {
    // A daemon predating this field (issue #187 review) emits `IndexJob`
    // JSON with no `error_code` key at all — `#[serde(default)]` must
    // still deserialize it, not fail the whole response.
    let json = r#"{
        "id": "job-1",
        "store_id": "store-1",
        "scope": {"type": "store"},
        "state": "failed",
        "stats": {},
        "error": "boom",
        "created_at": "2026-06-10T12:00:00Z"
    }"#;
    let job: IndexJob = serde_json::from_str(json).unwrap();
    assert_eq!(job.error_code, None);
    assert_eq!(job.error, Some("boom".to_string()));
}

#[test]
fn index_job_state_transitions() {
    let states = [
        IndexJobState::Pending,
        IndexJobState::Running,
        IndexJobState::Done,
        IndexJobState::Failed,
    ];
    for state in &states {
        let json = serde_json::to_value(state).unwrap();
        let back: IndexJobState = serde_json::from_value(json).unwrap();
        assert_eq!(*state, back);
    }
}

#[test]
fn index_job_state_serializes_lowercase() {
    let s = IndexJobState::Pending;
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        serde_json::json!("pending")
    );
    let s2 = IndexJobState::Done;
    assert_eq!(
        serde_json::to_value(&s2).unwrap(),
        serde_json::json!("done")
    );
}

#[test]
fn index_job_scope_source_roundtrip() {
    let scope = IndexJobScope::Source {
        source_id: new_ulid(),
    };
    let json = serde_json::to_string(&scope).unwrap();
    let scope2: IndexJobScope = serde_json::from_str(&json).unwrap();
    assert_eq!(scope, scope2);
}

#[test]
fn index_job_scope_document_roundtrip() {
    let doc_id = resource_id("file:///test.md", &content_hash("content"));
    let scope = IndexJobScope::Document {
        resource_id: doc_id,
    };
    let json = serde_json::to_string(&scope).unwrap();
    let scope2: IndexJobScope = serde_json::from_str(&json).unwrap();
    assert_eq!(scope, scope2);
}

#[test]
fn index_job_stats_default_all_zero() {
    let stats = IndexJobStats::default();
    assert_eq!(stats.docs_seen, 0);
    assert_eq!(stats.docs_indexed, 0);
    assert_eq!(stats.docs_deleted, 0);
    assert_eq!(stats.chunks_written, 0);
    assert_eq!(stats.unsupported_format_count, 0);
    assert_eq!(stats.error_count, 0);
}

// --- Span tests ---

#[test]
fn span_new_and_serialization() {
    let span = Span::new(10, 25);
    assert_eq!(span.start, 10);
    assert_eq!(span.end, 25);
    let json = serde_json::to_string(&span).unwrap();
    let span2: Span = serde_json::from_str(&json).unwrap();
    assert_eq!(span, span2);
}
