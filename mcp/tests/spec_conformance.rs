//! Spec <-> code conformance test for `get_chunks` (specs/05-surfaces.md §4.1).
//!
//! This test parses `specs/05-surfaces.md` *at test-run time* (via a
//! `CARGO_MANIFEST_DIR`-relative path), extracts the two worked JSON response
//! examples from §4.1, drives the real `get_chunks` tool (over a real
//! `rmcp` client/server pair, mirroring `mcp/tests/mcp_protocol.rs`'s
//! harness) with the equivalent scenario, and asserts the real response
//! conforms to the spec's documented shape.
//!
//! This is a **drift detector**: if a future change to `get_chunks`'s
//! response shape (an added/removed/renamed field) isn't mirrored in the
//! spec (or vice versa), this test fails. It intentionally does NOT import
//! anything from `mcp/tests/mcp_protocol.rs` (test files are separate
//! compilation units) — the small harness below is duplicated on purpose to
//! keep this file self-contained.
//!
//! Extraction is anchored on unambiguous prose markers within §4.1 (scoped
//! between the `### 4.1` and `### 4.2` headings so a match elsewhere in the
//! document can't be picked up by accident). If the spec's prose changes
//! shape enough that a marker can't be found, extraction panics with a
//! descriptive message — loudly, not silently skipped.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use rmcp::{
    model::CallToolRequestParams,
    model::CallToolResult,
    service::{RoleClient, RunningService},
    ServiceExt,
};

use localdb_core::{
    ids::{chunk_id, content_hash, new_ulid, resource_id},
    metadata::{DocumentMetadata, DublinCoreMetadata, Metadata},
    store::{ChunkRecord, FakeStore, RetrievalStore},
    types::Span,
    FakeEmbedder,
};
use mcp::{handler::McpHandler, AvailableStore, StaticStoreProvider, StoreDescriptor};

// ---------------------------------------------------------------------------
// Spec extraction
// ---------------------------------------------------------------------------

fn spec_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../specs/05-surfaces.md")
}

/// Slice `spec` down to the `### 4.1 ...` section (up to the next `### 4.2`
/// heading, or EOF), so later anchor lookups can't accidentally match text
/// from an unrelated section.
fn section_4_1(spec: &str) -> &str {
    const START: &str = "### 4.1 `get_chunks`";
    const END: &str = "### 4.2";
    let start = spec.find(START).unwrap_or_else(|| {
        panic!(
            "could not find {START:?} heading in specs/05-surfaces.md — \
             the spec's structure has changed; update this test's anchors"
        )
    });
    let rest = &spec[start..];
    match rest.find(END) {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// Find `marker` in `text`, then return the contents of the next fenced
/// ` ```json ` block after it (exclusive of the fences). Panics loudly
/// (test failure, not a skip) if either the marker or the fence can't be
/// found, since that means the spec's prose no longer matches the shape
/// this test expects.
fn json_block_after<'a>(text: &'a str, marker: &str) -> &'a str {
    let marker_pos = text.find(marker).unwrap_or_else(|| {
        panic!(
            "could not find anchor text {marker:?} in specs/05-surfaces.md §4.1 — \
             the spec's prose has changed; update this test's anchors"
        )
    });
    let after_marker = &text[marker_pos + marker.len()..];
    const FENCE: &str = "```json";
    let fence_start = after_marker.find(FENCE).unwrap_or_else(|| {
        panic!(
            "found anchor {marker:?} but no fenced ```json block followed it \
             in specs/05-surfaces.md §4.1"
        )
    });
    let after_fence = &after_marker[fence_start + FENCE.len()..];
    let fence_end = after_fence
        .find("```")
        .unwrap_or_else(|| panic!("fenced ```json block after anchor {marker:?} was never closed"));
    after_fence[..fence_end].trim()
}

/// Extracted §4.1 example JSON blocks, parsed once per test.
struct SpecExamples {
    /// The first response-shape JSON block in §4.1 (plain `offset` pagination).
    plain_offset_shape: Value,
    /// The worked anchor-pagination example's response JSON (20 chunks,
    /// `anchor_chunk_id` at `block_seq = 10`, `limit: 5`).
    anchor_example_response: Value,
}

fn extract_spec_examples() -> SpecExamples {
    let spec = std::fs::read_to_string(spec_path()).unwrap_or_else(|e| {
        panic!(
            "specs/05-surfaces.md should exist and be readable at {:?}: {e}",
            spec_path()
        )
    });
    let section = section_4_1(&spec);

    let plain_offset_raw = json_block_after(section, "Response shape (plain `offset` pagination):");
    let plain_offset_shape: Value = serde_json::from_str(plain_offset_raw).unwrap_or_else(|e| {
        panic!("plain-offset response-shape block did not parse as JSON: {e}\nblock:\n{plain_offset_raw}")
    });

    // Scope the anchor-example lookup to *after* "**Anchor example:**" so we
    // can't accidentally grab the plain-offset block's own "Response" text
    // (there is none — the plain-offset block is introduced by "Response
    // shape (plain `offset` pagination):", not "Response:" — but scoping
    // explicitly keeps the anchor future-proof regardless).
    const ANCHOR_MARKER: &str = "**Anchor example:**";
    let anchor_marker_pos = section.find(ANCHOR_MARKER).unwrap_or_else(|| {
        panic!(
            "could not find {ANCHOR_MARKER:?} in specs/05-surfaces.md §4.1 — \
             the spec's worked example prose has changed; update this test's anchors"
        )
    });
    let anchor_section = &section[anchor_marker_pos..];
    let anchor_example_raw = json_block_after(anchor_section, "Response:");
    let anchor_example_response: Value =
        serde_json::from_str(anchor_example_raw).unwrap_or_else(|e| {
            panic!(
            "anchor-example response block did not parse as JSON: {e}\nblock:\n{anchor_example_raw}"
        )
        });

    SpecExamples {
        plain_offset_shape,
        anchor_example_response,
    }
}

// ---------------------------------------------------------------------------
// Generic shape matcher
// ---------------------------------------------------------------------------

/// Recursively assert that `real` has the same *shape* as `spec`:
///
/// - object keys match as a SET at every nesting level (the drift
///   detector — an extra or missing key anywhere is a failure);
/// - a spec array of example objects (e.g. the worked `chunks` list)
///   requires at least one real element, and every real element is
///   checked against the FIRST spec element's shape (recursively);
/// - a spec array of bare placeholder strings (e.g. `heading_path:
///   ["..."]`) only requires real elements (if any) to be strings;
/// - a spec placeholder string value (`"..."`) matches any real string;
/// - concrete (non-placeholder) leaf values — numbers, null, literal
///   strings — are NOT compared here (several numeric fields in the spec's
///   JSON, e.g. `span.start`/`span.end`, are illustrative zeros rather than
///   literal worked-example values); callers assert those explicitly where
///   the spec text calls them out as concrete (`total_chunks`, `offset`,
///   `limit`, `returned`, `anchor_index`, `block_seq`).
fn assert_matches_spec_shape(spec: &Value, real: &Value, path: &str) {
    match spec {
        Value::Object(smap) => {
            let rmap = match real {
                Value::Object(m) => m,
                other => panic!("{path}: expected an object in the real response, got {other:?}"),
            };
            let skeys: BTreeSet<&String> = smap.keys().collect();
            let rkeys: BTreeSet<&String> = rmap.keys().collect();
            assert_eq!(
                skeys,
                rkeys,
                "key set mismatch at `{path}` (spec vs. real get_chunks response) — \
                 spec-only keys: {:?}, real-only keys: {:?}",
                skeys.difference(&rkeys).collect::<Vec<_>>(),
                rkeys.difference(&skeys).collect::<Vec<_>>(),
            );
            for (k, sv) in smap {
                assert_matches_spec_shape(sv, &rmap[k], &format!("{path}.{k}"));
            }
        }
        Value::Array(sarr) => {
            let rarr = match real {
                Value::Array(a) => a,
                other => panic!("{path}: expected an array in the real response, got {other:?}"),
            };
            if sarr.is_empty() {
                return;
            }
            if sarr.iter().all(Value::is_object) {
                assert!(
                    !rarr.is_empty(),
                    "{path}: spec shows example element(s) but the real response's array is empty \
                     — the test scenario must produce at least one element to check shape against"
                );
                let template = &sarr[0];
                for (i, rv) in rarr.iter().enumerate() {
                    assert_matches_spec_shape(template, rv, &format!("{path}[{i}]"));
                }
            } else {
                for (i, rv) in rarr.iter().enumerate() {
                    assert!(
                        rv.is_string(),
                        "{path}[{i}]: expected a string (matching spec placeholder array), got {rv:?}"
                    );
                }
            }
        }
        Value::String(s) if s == "..." => {
            assert!(
                real.is_string(),
                "{path}: spec placeholder `\"...\"` must match a real string, got {real:?}"
            );
        }
        // Literal (non-"...") strings, numbers, null, and bools: this
        // function only checks shape (key sets / array-ness / placeholder
        // strings). Concrete-value equality for fields the spec text calls
        // out as worked-example values is asserted separately by the caller.
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// MCP test harness (duplicated from mcp/tests/mcp_protocol.rs on purpose —
// each file under tests/ is its own compilation unit).
// ---------------------------------------------------------------------------

async fn client_for(handler: McpHandler) -> RunningService<RoleClient, ()> {
    let (server_transport, client_transport) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        match handler.serve(server_transport).await {
            Ok(running) => {
                let _ = running.waiting().await;
            }
            Err(e) => panic!("server failed to initialize: {e}"),
        }
    });
    ().serve(client_transport)
        .await
        .expect("client should connect")
}

async fn call_tool(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    arguments: Value,
) -> CallToolResult {
    let args = arguments
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args))
        .await
        .expect("get_chunks call should succeed at the protocol level")
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("expected a text content item")
}

/// Build a handler seeded with ONE document of `count` chunks, one per
/// block (`block_seq` 0..count, `seq_in_block` 0) — mirrors the shape of
/// the spec's worked anchor-pagination example (specs/05-surfaces.md §4.1:
/// 20 chunks, one chunk per block). Unlike `mcp_protocol.rs`'s equivalent
/// helper, every chunk here carries a non-null `title` (via document
/// metadata), a non-null `block_kind`, and a non-empty `heading_path` —
/// every field the spec's worked example shows as a placeholder STRING
/// (`"..."`) must be a real string in the response, not null, for the
/// shape-matcher's placeholder-string check to mean anything.
async fn make_handler_with_sequential_chunks(count: u32) -> (McpHandler, String, Vec<String>) {
    let store = std::sync::Arc::new(FakeStore::new());

    let uri = "file:///docs/sequential.md";
    let doc_hash = content_hash("sequential document body");
    let doc_id = resource_id(uri, &doc_hash);

    let metadata = Metadata::Document(DocumentMetadata {
        dublin_core: DublinCoreMetadata {
            title: Some("Sequential Doc".to_string()),
            ..Default::default()
        },
        ..Default::default()
    });

    let mut chunks = Vec::new();
    let mut ids = Vec::new();
    for block_seq in 0..count {
        let text = format!("chunk body {block_seq}");
        let cid = chunk_id(&doc_id, block_seq, &text, 0);
        ids.push(cid.clone());
        chunks.push(ChunkRecord {
            id: cid,
            resource_id: doc_id.clone(),
            store_id: "store-1".to_string(),
            text: text.clone(),
            span: Span::new(0, text.len()),
            heading_path: vec!["Body".to_string()],
            embedding: vec![0.1, 0.2, 0.3, 0.4],
            policy_version: "v1".to_string(),
            fetched_at: "2026-06-10T12:00:00Z".to_string(),
            content_hash: doc_hash.clone(),
            origin_store: "store-1".to_string(),
            source_id: new_ulid(),
            ingestor_kind: "path".to_string(),
            mime: Some("text/markdown".to_string()),
            uri: uri.to_string(),
            metadata: metadata.clone(),
            block_seq,
            seq_in_block: 0,
            block_kind: Some("text".to_string()),
            window_block_seqs: vec![],
        });
    }
    store.upsert_chunks(chunks).await.expect("seed chunks");

    let sd = StoreDescriptor {
        id: "store-1".to_string(),
        name: "test-store".to_string(),
        visibility: "private".to_string(),
    };
    let available = AvailableStore::from_arc(sd, store);
    let embedder: std::sync::Arc<dyn localdb_core::Embedder> =
        std::sync::Arc::new(FakeEmbedder::new(4));
    let handler = McpHandler::new(
        std::sync::Arc::new(StaticStoreProvider::new(vec![available])),
        embedder,
        false,
        Some(localdb_core::auth::Principal::local_trust()),
    );
    (handler, doc_id, ids)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Spec <-> code conformance for the §4.1 worked anchor-pagination example:
/// a 20-chunk resource, `anchor_chunk_id` at `block_seq = 10`, `limit: 5`
/// -> `offset: 8`, `anchor_index: 2`, chunks covering `block_seq` 8-12.
///
/// Checks, per the task's acceptance criteria:
/// - identical key sets at every nesting level (drift detector);
/// - identical concrete values the spec calls out as worked-example facts
///   (`total_chunks`, `offset`, `limit`, `returned`, `anchor_index`, and
///   each returned chunk's `block_seq`);
/// - spec placeholder strings (`"..."`) match any real string.
#[tokio::test]
async fn get_chunks_anchor_example_matches_spec_05_surfaces_4_1() {
    let examples = extract_spec_examples();
    let spec = &examples.anchor_example_response;

    // Sanity: confirm we actually extracted the worked example described in
    // the task (20 chunks, anchor at block_seq 10 -> offset 8, anchor_index
    // 2) and not some other JSON block — if this fails, the extraction
    // anchors grabbed the wrong block.
    assert_eq!(
        spec["total_chunks"], 20,
        "unexpected block extracted (total_chunks)"
    );
    assert_eq!(spec["offset"], 8, "unexpected block extracted (offset)");
    assert_eq!(spec["limit"], 5, "unexpected block extracted (limit)");
    assert_eq!(spec["returned"], 5, "unexpected block extracted (returned)");
    assert_eq!(
        spec["anchor_index"], 2,
        "unexpected block extracted (anchor_index)"
    );

    let (handler, doc_id, ids) = make_handler_with_sequential_chunks(20).await;
    let client = client_for(handler).await;

    let anchor_id = ids[10].clone();
    let result = call_tool(
        &client,
        "get_chunks",
        serde_json::json!({ "resource_id": doc_id, "anchor_chunk_id": anchor_id, "limit": 5 }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "get_chunks should succeed");

    let real: Value = serde_json::from_str(&text_of(&result)).expect("valid JSON in content");

    // 1 + 3: structural (key-set) and placeholder-string conformance.
    assert_matches_spec_shape(spec, &real, "get_chunks_response");

    // 2: concrete worked-example values, read from the spec itself (not
    // re-transcribed) so this stays correct if the spec's prose changes the
    // numbers but keeps the same shape.
    for key in [
        "total_chunks",
        "offset",
        "limit",
        "returned",
        "anchor_index",
    ] {
        assert_eq!(
            real[key], spec[key],
            "concrete field `{key}` differs between spec example and real response"
        );
    }

    let spec_chunks = spec["chunks"].as_array().expect("spec chunks array");
    let real_chunks = real["chunks"].as_array().expect("real chunks array");
    assert_eq!(
        spec_chunks.len(),
        real_chunks.len(),
        "chunk count differs between spec example and real response"
    );
    for (i, (s, r)) in spec_chunks.iter().zip(real_chunks.iter()).enumerate() {
        assert_eq!(
            r["block_seq"], s["block_seq"],
            "chunks[{i}].block_seq differs between spec example and real response"
        );
    }
}

/// Spec <-> code conformance for the plain-`offset` pagination response
/// shape (the first response-shape JSON block in §4.1) against a real
/// plain-offset `get_chunks` call. All of that block's values are
/// placeholders/zeros (it documents shape, not a worked example), so per
/// the task spec only key sets (and placeholder-string typing) are
/// checked — no concrete-value equality.
#[tokio::test]
async fn get_chunks_plain_offset_shape_matches_spec_05_surfaces_4_1() {
    let examples = extract_spec_examples();
    let spec = &examples.plain_offset_shape;

    // Sanity: this block's anchor_index must be `null` (plain-offset mode)
    // — if this fails, extraction grabbed the anchor-example block instead.
    assert!(
        spec["anchor_index"].is_null(),
        "unexpected block extracted: plain-offset shape should have anchor_index: null"
    );

    let (handler, doc_id, _ids) = make_handler_with_sequential_chunks(3).await;
    let client = client_for(handler).await;

    // No offset/limit/anchor args at all -> plain-offset defaults, at least
    // one chunk returned so the array-shape check below has something to
    // check element shape against.
    let result = call_tool(
        &client,
        "get_chunks",
        serde_json::json!({ "resource_id": doc_id }),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "get_chunks should succeed");

    let real: Value = serde_json::from_str(&text_of(&result)).expect("valid JSON in content");
    assert!(
        !real["chunks"].as_array().unwrap_or(&Vec::new()).is_empty(),
        "test scenario must return at least one chunk to check element shape"
    );

    assert_matches_spec_shape(spec, &real, "get_chunks_response");
}
