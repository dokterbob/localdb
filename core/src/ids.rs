//! ID derivation for localdb domain entities.
//!
//! Two classes of IDs:
//! - **ULID** for entities that exist by fiat (Store, Source, IndexJob).
//! - **Content-addressed blake3** for entities derived from content (Document, Chunk).
//!
//! Content-addressed IDs are the federation prerequisite — two nodes indexing the same
//! content derive the same chunk identity, enabling dedup, provenance comparison, and
//! integrity checks without coordination.

use ulid::Ulid;

use crate::metadata::Metadata;

/// A ULID as a string, used for fiat-identity entities (Store, Source, IndexJob).
///
/// The string representation is the canonical form; it is stable, sortable by time,
/// and safe to use as a database key.
pub type UlidId = String;

/// A blake3 content-addressed ID as a hex string.
///
/// Used for Document and Chunk, where identity is derived from content.
pub type ContentId = String;

/// Generate a new ULID for a fiat-identity entity.
pub fn new_ulid() -> UlidId {
    Ulid::new().to_string()
}

/// Derive a content-addressed ID for a Document.
///
/// The ID is `blake3(canonical_source_uri || content_hash)`.
/// Both inputs must be deterministic given the same content and source.
///
/// # Arguments
/// * `canonical_source_uri` - The canonical URI of the source (e.g. `file:///path/to/file`).
/// * `content_hash` - The blake3 hash of the normalized extracted text (as hex string).
pub fn resource_id(canonical_source_uri: &str, content_hash: &str) -> ContentId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(canonical_source_uri.as_bytes());
    hasher.update(content_hash.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Derive a content hash for document content.
///
/// Takes the normalized extracted text and returns a blake3 hex hash.
/// This drives incremental re-index decisions.
pub fn content_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// Derive a content-addressed ID for a Chunk.
///
/// The ID is `blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)`, hashed in
/// exactly that order. Stable across re-runs over identical content.
///
/// Span (byte offsets) is deliberately NOT an input: spans are block-relative and can
/// shift slightly between runs (e.g. due to whitespace-normalization tweaks) without the
/// chunk's actual membership changing, which would otherwise needlessly churn IDs. Instead,
/// `block_seq` and `seq_in_block` — the chunk's *final* position once block dispatch and any
/// fix-up pass (e.g. the message-window shrink-to-fit) have settled — anchor identity, so IDs
/// must be computed only after both are final. See specs/02-domain-model.md §2/§3 and
/// specs/04-search-pipeline.md §3.
///
/// Integer components are hashed as little-endian bytes (`u32::to_le_bytes`) — an arbitrary
/// but fixed choice; what matters is that it never changes without a policy-version bump
/// (`core::config::policy`).
///
/// # Arguments
/// * `resource_id` - The content-addressed ID of the parent document.
/// * `block_seq` - Sequence number of the parent block within the resource. Prevents
///   collisions when two blocks in the same document contain identical chunk text, and
///   distinguishes message-window chunks (keyed off their first member block's seq).
/// * `chunk_text` - The text content of the chunk.
/// * `seq_in_block` - The chunk's final position within its block (0-indexed). Distinguishes
///   multiple chunks produced from the same block that happen to share text.
pub fn chunk_id(
    resource_id: &str,
    block_seq: u32,
    chunk_text: &str,
    seq_in_block: u32,
) -> ContentId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(resource_id.as_bytes());
    hasher.update(&block_seq.to_le_bytes());
    hasher.update(chunk_text.as_bytes());
    hasher.update(&seq_in_block.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Derive a hash over a resource's persisted metadata state (specs/04-search-pipeline.md,
/// metadata-only incremental update).
///
/// Inputs are exactly the fields `store-libsql` persists on the `resources`
/// row alongside `metadata_json` — `external_id`, `external_etag` — plus
/// `metadata` itself. Callers MUST pass post-backfill, exactly-as-persisted
/// values: `metadata` after `core::ingestion`'s title-backfill (a resource's
/// own `title` folds into `metadata.dublin_core_mut().title` when the
/// metadata carries none), never the raw `resource.metadata`. Computing this
/// before backfill at one call site and after backfill at another would make
/// the same resource hash differently depending on which code path touched
/// it first — exactly the bug this hash exists to avoid across index-time,
/// metadata-update-time, and rehydration-time computations.
///
/// `modified_at` IS included, as the source's claim (`Option<&str>`, not the
/// `now()` fallback this hash used to have to defend against — see below).
///
/// Deliberately excludes:
/// - `title` as a separate parameter — it always lives inside `metadata` by
///   the time this is called (see above), so a separate parameter would
///   double-count it.
/// - `date_original`/`date_parsed` — both are derived from `metadata`'s own
///   Dublin Core `date` field (`core::dates::parse_partial_iso8601`), so
///   hashing them too would double-count that field a second time.
///
/// `modified_at` used to be excluded here: for a source with no change claim
/// of its own it used to fall back to ingestion-time `now()`, which would
/// make otherwise-identical content hash differently on every single run and
/// permanently defeat the unchanged-skip path. Now that `Resource::modified_at`
/// (and every field it flows through, down to the nullable
/// `resources.modified_at` column) is `Option<String>` — `None` when the
/// source makes no claim, never `now()` — that churn source is gone: a
/// no-claim source hashes a stable `None` on every run, so it's safe (and
/// correct — a genuine claim change IS a metadata change) to include it.
///
/// `metadata` must serialize deterministically for this to be stable: every
/// `Metadata` variant is plain structs/`Vec`s/`Option`s with no `HashMap`, so
/// `serde_json::to_string` produces the same bytes for equal values on every
/// call — a `HashMap`-valued field would risk nondeterministic key ordering
/// and make this hash spuriously flap between runs with no real change.
///
/// Fields are combined with `\x00` separators (mirroring
/// `markdown_blocks::compute_blocks_hash`'s delimiting convention) so a
/// shifted field boundary — e.g. `external_id="ab"` + `external_etag="c"` vs
/// `external_id="a"` + `external_etag="bc"` — can never collide. Each
/// optional field is additionally tagged with a `\x01` marker when present
/// (same convention `compute_blocks_hash` uses for its optional page
/// suffix), so `None` and `Some("")` can never collide either — otherwise
/// `external_id.unwrap_or("")` would hash a present-but-empty value
/// identically to an absent one.
pub fn compute_metadata_hash(
    metadata: &Metadata,
    external_id: Option<&str>,
    external_etag: Option<&str>,
    modified_at: Option<&str>,
) -> String {
    let metadata_json = serde_json::to_string(metadata).unwrap_or_default();
    let combined = format!(
        "{}\x00{}\x00{}\x00{}",
        metadata_json,
        encode_optional_field(external_id),
        encode_optional_field(external_etag),
        encode_optional_field(modified_at),
    );
    content_hash(&combined)
}

/// Digest of the **local** inputs that decide what a feed run produces from
/// an unchanged feed document, stored on `sources.feed_inputs_digest`.
///
/// A conditional GET rests on the origin rotating its validator whenever its
/// own representation changes (RFC 9110 §8.8.1). That contract binds the
/// origin and nothing else — it knows nothing about our indexing policy, or
/// whether we follow entry links, or how many entries we take. Without this
/// digest, changing any of those against an unchanged feed produces a 304,
/// the entry loop never runs, and not one entry is reprocessed under the new
/// inputs. Comparing it before replaying the stored validators is what
/// closes that. See `specs/02-domain-model.md`'s Feed connector,
/// "Conditional GET and pruning".
///
/// The three inputs are exactly those that change a run's *output* for the
/// same bytes. `refresh_interval_secs` is deliberately absent: it changes
/// when a run happens, never what it produces. So is the feed URL — a
/// changed URL is a new origin, and `upsert_source` nulls the whole cache
/// for that case rather than relying on a digest comparison to catch it.
///
/// Same encoding discipline as [`compute_metadata_hash`]: `\x00` separators
/// and [`encode_optional_field`], so an unbounded `max_entries` cannot
/// collide with any bounded one.
pub fn compute_feed_inputs_digest(
    policy_version: &str,
    fetch_full_content: bool,
    max_entries: Option<u32>,
) -> String {
    let combined = format!(
        "{}\x00{}\x00{}",
        policy_version,
        fetch_full_content,
        encode_optional_field(max_entries.map(|n| n.to_string()).as_deref()),
    );
    content_hash(&combined)
}

/// Encode an optional hash-input field so `None` and `Some("")` produce
/// distinct output: `Some(v)` becomes `\x01v`, `None` becomes the empty
/// string. See [`compute_metadata_hash`]'s doc comment.
fn encode_optional_field(value: Option<&str>) -> String {
    match value {
        Some(v) => format!("\x01{v}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{DocumentMetadata, DublinCoreMetadata};

    // --- Failing tests first (TDD) ---

    #[test]
    fn ulid_is_non_empty() {
        let id = new_ulid();
        assert!(!id.is_empty());
    }

    #[test]
    fn ulids_are_unique() {
        let a = new_ulid();
        let b = new_ulid();
        assert_ne!(a, b, "two ULIDs must not collide");
    }

    #[test]
    fn ulid_is_26_chars() {
        // ULID canonical string representation is always 26 characters
        let id = new_ulid();
        assert_eq!(id.len(), 26, "ULID should be 26 chars, got: {id}");
    }

    // --- Document ID stability ---

    #[test]
    fn same_content_produces_same_resource_id() {
        let uri = "file:///home/user/docs/notes.md";
        let hash = content_hash("Hello, world!");
        let id1 = resource_id(uri, &hash);
        let id2 = resource_id(uri, &hash);
        assert_eq!(id1, id2, "same content must produce same document ID");
    }

    #[test]
    fn different_content_produces_different_resource_id() {
        let uri = "file:///home/user/docs/notes.md";
        let hash1 = content_hash("Hello, world!");
        let hash2 = content_hash("Goodbye, world!");
        let id1 = resource_id(uri, &hash1);
        let id2 = resource_id(uri, &hash2);
        assert_ne!(
            id1, id2,
            "changed content must produce a different document ID"
        );
    }

    #[test]
    fn different_uri_produces_different_resource_id() {
        let hash = content_hash("Same content");
        let id1 = resource_id("file:///path/a.md", &hash);
        let id2 = resource_id("file:///path/b.md", &hash);
        assert_ne!(
            id1, id2,
            "different URIs with same content must produce different document IDs"
        );
    }

    #[test]
    fn resource_id_is_hex_string() {
        let hash = content_hash("test");
        let id = resource_id("file:///test.md", &hash);
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "document ID must be a hex string"
        );
        assert_eq!(id.len(), 64, "blake3 hex output is 64 chars");
    }

    // --- Content hash stability ---

    #[test]
    fn same_text_produces_same_content_hash() {
        let h1 = content_hash("Hello, world!");
        let h2 = content_hash("Hello, world!");
        assert_eq!(h1, h2, "same text must produce same content hash");
    }

    #[test]
    fn different_text_produces_different_content_hash() {
        let h1 = content_hash("Hello, world!");
        let h2 = content_hash("Hello, World!"); // capital W
        assert_ne!(h1, h2, "different text must produce different content hash");
    }

    #[test]
    fn empty_text_has_stable_hash() {
        let h1 = content_hash("");
        let h2 = content_hash("");
        assert_eq!(h1, h2);
    }

    // --- Chunk ID stability ---

    #[test]
    fn same_content_produces_same_chunk_id() {
        let doc_id = resource_id("file:///notes.md", &content_hash("doc text"));
        let id1 = chunk_id(&doc_id, 0, "chunk text here", 0);
        let id2 = chunk_id(&doc_id, 0, "chunk text here", 0);
        assert_eq!(id1, id2, "same inputs must produce same chunk ID");
    }

    #[test]
    fn changed_chunk_text_produces_different_chunk_id() {
        let doc_id = resource_id("file:///notes.md", &content_hash("doc text"));
        let id1 = chunk_id(&doc_id, 0, "original chunk", 0);
        let id2 = chunk_id(&doc_id, 0, "modified chunk", 0);
        assert_ne!(id1, id2, "changed chunk text must produce different ID");
    }

    #[test]
    fn changed_seq_in_block_produces_different_chunk_id() {
        let doc_id = resource_id("file:///notes.md", &content_hash("doc text"));
        let text = "chunk text";
        let id1 = chunk_id(&doc_id, 0, text, 0);
        let id2 = chunk_id(&doc_id, 0, text, 1); // different seq_in_block
        assert_ne!(
            id1, id2,
            "changed seq_in_block must produce different chunk ID"
        );
    }

    #[test]
    fn changed_resource_id_produces_different_chunk_id() {
        let doc_id1 = resource_id("file:///doc1.md", &content_hash("content1"));
        let doc_id2 = resource_id("file:///doc2.md", &content_hash("content2"));
        let id1 = chunk_id(&doc_id1, 0, "same text", 0);
        let id2 = chunk_id(&doc_id2, 0, "same text", 0);
        assert_ne!(
            id1, id2,
            "different document must produce different chunk ID"
        );
    }

    #[test]
    fn chunk_id_is_hex_string() {
        let doc_id = resource_id("file:///test.md", &content_hash("test"));
        let id = chunk_id(&doc_id, 0, "chunk", 0);
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "chunk ID must be a hex string"
        );
        assert_eq!(id.len(), 64, "blake3 hex output is 64 chars");
    }

    #[test]
    fn different_block_seq_produces_different_chunk_id() {
        let doc_id = resource_id("file:///notes.md", &content_hash("doc text"));
        // Same text and seq_in_block, different block_seq → different IDs.
        let id1 = chunk_id(&doc_id, 0, "identical text", 0);
        let id2 = chunk_id(&doc_id, 1, "identical text", 0);
        assert_ne!(
            id1, id2,
            "different block_seq must produce different chunk ID"
        );
    }

    // --- Cross-type stability ---

    #[test]
    fn resource_id_stable_across_reruns() {
        // Simulates indexing the same file twice
        let uri = "file:///data/report.md";
        let text = "# Report\n\nSome content here.";
        let hash = content_hash(text);
        let id_run1 = resource_id(uri, &hash);
        let id_run2 = resource_id(uri, &hash);
        assert_eq!(
            id_run1, id_run2,
            "document ID must be stable across re-runs"
        );
    }

    #[test]
    fn chunk_id_stable_across_reruns() {
        let uri = "file:///data/report.md";
        let text = "# Report\n\nSome content here.";
        let hash = content_hash(text);
        let doc_id = resource_id(uri, &hash);
        let chunk_text = "Some content here.";
        let id_run1 = chunk_id(&doc_id, 0, chunk_text, 0);
        let id_run2 = chunk_id(&doc_id, 0, chunk_text, 0);
        assert_eq!(id_run1, id_run2, "chunk ID must be stable across re-runs");
    }

    // --- compute_metadata_hash ---

    fn doc_metadata(title: Option<&str>) -> Metadata {
        Metadata::Document(DocumentMetadata {
            dublin_core: DublinCoreMetadata {
                title: title.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    #[test]
    fn metadata_hash_is_deterministic() {
        let m = doc_metadata(Some("Title"));
        let h1 = compute_metadata_hash(&m, Some("ext-1"), Some("etag-1"), None);
        let h2 = compute_metadata_hash(&m, Some("ext-1"), Some("etag-1"), None);
        assert_eq!(h1, h2);
    }

    #[test]
    fn metadata_hash_is_hex_encoded_blake3() {
        let m = doc_metadata(None);
        let h = compute_metadata_hash(&m, None, None, None);
        assert_eq!(h.len(), 64, "expected 64-char hex string, got: {h}");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn metadata_hash_changes_when_metadata_changes() {
        let a = doc_metadata(Some("A"));
        let b = doc_metadata(Some("B"));
        let ha = compute_metadata_hash(&a, None, None, None);
        let hb = compute_metadata_hash(&b, None, None, None);
        assert_ne!(ha, hb);
    }

    #[test]
    fn metadata_hash_changes_when_external_id_changes() {
        let m = doc_metadata(Some("Title"));
        let h1 = compute_metadata_hash(&m, Some("a"), None, None);
        let h2 = compute_metadata_hash(&m, Some("b"), None, None);
        assert_ne!(h1, h2);
    }

    #[test]
    fn metadata_hash_changes_when_external_etag_changes() {
        let m = doc_metadata(Some("Title"));
        let h1 = compute_metadata_hash(&m, None, Some("a"), None);
        let h2 = compute_metadata_hash(&m, None, Some("b"), None);
        assert_ne!(h1, h2);
    }

    #[test]
    fn metadata_hash_field_boundary_is_unambiguous() {
        // external_id="ab" + external_etag="c" must not collide with
        // external_id="a" + external_etag="bc" — the `\x00` separator must
        // hold the field boundary even when a naive concatenation wouldn't.
        let m = doc_metadata(None);
        let h1 = compute_metadata_hash(&m, Some("ab"), Some("c"), None);
        let h2 = compute_metadata_hash(&m, Some("a"), Some("bc"), None);
        assert_ne!(h1, h2);
    }

    /// F3: `external_id: None` must not hash the same as
    /// `external_id: Some("")` — a naive `unwrap_or("")` would collide the
    /// two. Each optional field is `\x01`-tagged when present (see the doc
    /// comment), so an absent field and a present-but-empty one can never
    /// produce the same combined string.
    #[test]
    fn metadata_hash_distinguishes_none_from_empty_external_fields() {
        let m = doc_metadata(Some("Title"));
        let h_none = compute_metadata_hash(&m, None, None, None);
        let h_empty = compute_metadata_hash(&m, Some(""), Some(""), None);
        assert_ne!(
            h_none, h_empty,
            "external_id/external_etag of None must hash differently from Some(\"\")"
        );
    }

    /// The trap this hash exists to pin closed: a resource whose title was
    /// BACKFILLED (`resource.title` carries it, `resource.metadata`'s Dublin
    /// Core title does not) must hash identically whether the caller passes
    /// the pre-backfill or the already-backfilled `Metadata` — as long as
    /// both actually contain the title. This test documents the contract at
    /// the unit level; `core/tests/metadata_skip.rs`
    /// (`list_indexed_documents_metadata_hash_matches_index_resource_stamped_hash`)
    /// pins it end-to-end across index-time and rehydration-time.
    #[test]
    fn metadata_hash_treats_backfilled_title_as_ordinary_metadata() {
        let backfilled = doc_metadata(Some("Backfilled Title"));
        let h1 = compute_metadata_hash(&backfilled, None, None, None);
        let h2 = compute_metadata_hash(&backfilled, None, None, None);
        assert_eq!(
            h1, h2,
            "hashing post-backfill metadata must be deterministic"
        );
    }

    /// The re-inclusion this whole change is about (#283): a genuine claim
    /// change must move the hash.
    #[test]
    fn metadata_hash_changes_when_modified_at_changes() {
        let m = doc_metadata(Some("Title"));
        let h_none = compute_metadata_hash(&m, None, None, None);
        let h_claim = compute_metadata_hash(&m, None, None, Some("2020-01-01T00:00:00Z"));
        assert_ne!(
            h_none, h_claim,
            "a source claim must hash differently from no claim"
        );
    }

    /// No-claim sources must hash a stable `None` on every run — the F1 bug
    /// this design fixes: hashing `now()` instead would churn on every run
    /// even with unchanged content and metadata.
    #[test]
    fn metadata_hash_none_modified_at_is_stable_across_calls() {
        let m = doc_metadata(Some("Title"));
        let h1 = compute_metadata_hash(&m, None, None, None);
        let h2 = compute_metadata_hash(&m, None, None, None);
        assert_eq!(h1, h2, "None modified_at must hash identically every run");
    }

    /// `modified_at: None` must not hash the same as `modified_at: Some("")`
    /// — same F3 collision guard as `external_id`/`external_etag` above. In
    /// practice a real claim is always a non-empty RFC 3339 string and the
    /// nullable `resources.modified_at` column round-trips `None` as SQL
    /// NULL, so the two can't collide upstream either — this pins the
    /// encoding property at the hash-function level regardless.
    #[test]
    fn metadata_hash_distinguishes_none_from_empty_modified_at() {
        let m = doc_metadata(Some("Title"));
        let h_none = compute_metadata_hash(&m, None, None, None);
        let h_empty = compute_metadata_hash(&m, None, None, Some(""));
        assert_ne!(
            h_none, h_empty,
            "modified_at of None must hash differently from Some(\"\")"
        );
    }

    // -----------------------------------------------------------------
    // compute_feed_inputs_digest
    // -----------------------------------------------------------------

    #[test]
    fn feed_inputs_digest_is_stable_for_identical_inputs() {
        assert_eq!(
            compute_feed_inputs_digest("policy-v1", true, Some(10)),
            compute_feed_inputs_digest("policy-v1", true, Some(10))
        );
    }

    #[test]
    fn feed_inputs_digest_changes_with_each_input_independently() {
        let base = compute_feed_inputs_digest("policy-v1", true, Some(10));
        assert_ne!(
            base,
            compute_feed_inputs_digest("policy-v2", true, Some(10))
        );
        assert_ne!(
            base,
            compute_feed_inputs_digest("policy-v1", false, Some(10))
        );
        assert_ne!(base, compute_feed_inputs_digest("policy-v1", true, Some(5)));
        assert_ne!(base, compute_feed_inputs_digest("policy-v1", true, None));
    }

    #[test]
    fn feed_inputs_digest_separators_prevent_field_bleed() {
        // Without the \x00 separators these two would concatenate to the
        // same string, and narrowing max_entries while renaming the policy
        // could silently keep replaying a stale validator.
        assert_ne!(
            compute_feed_inputs_digest("a", true, Some(12)),
            compute_feed_inputs_digest("a", true, Some(1))
        );
        assert_ne!(
            compute_feed_inputs_digest("ab", true, None),
            compute_feed_inputs_digest("a", true, None)
        );
    }
}
