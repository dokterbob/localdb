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
pub fn document_id(canonical_source_uri: &str, content_hash: &str) -> ContentId {
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
/// The ID is `blake3(document_id || chunk_text || span_start || span_end)`.
/// Stable across re-runs over identical content.
///
/// # Arguments
/// * `document_id` - The content-addressed ID of the parent document.
/// * `chunk_text` - The text content of the chunk.
/// * `span_start` - Start byte offset in the normalized document text.
/// * `span_end` - End byte offset in the normalized document text.
pub fn chunk_id(
    document_id: &str,
    chunk_text: &str,
    span_start: usize,
    span_end: usize,
) -> ContentId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(document_id.as_bytes());
    hasher.update(chunk_text.as_bytes());
    hasher.update(&span_start.to_le_bytes());
    hasher.update(&span_end.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn same_content_produces_same_document_id() {
        let uri = "file:///home/user/docs/notes.md";
        let hash = content_hash("Hello, world!");
        let id1 = document_id(uri, &hash);
        let id2 = document_id(uri, &hash);
        assert_eq!(id1, id2, "same content must produce same document ID");
    }

    #[test]
    fn different_content_produces_different_document_id() {
        let uri = "file:///home/user/docs/notes.md";
        let hash1 = content_hash("Hello, world!");
        let hash2 = content_hash("Goodbye, world!");
        let id1 = document_id(uri, &hash1);
        let id2 = document_id(uri, &hash2);
        assert_ne!(
            id1, id2,
            "changed content must produce a different document ID"
        );
    }

    #[test]
    fn different_uri_produces_different_document_id() {
        let hash = content_hash("Same content");
        let id1 = document_id("file:///path/a.md", &hash);
        let id2 = document_id("file:///path/b.md", &hash);
        assert_ne!(
            id1, id2,
            "different URIs with same content must produce different document IDs"
        );
    }

    #[test]
    fn document_id_is_hex_string() {
        let hash = content_hash("test");
        let id = document_id("file:///test.md", &hash);
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
        let doc_id = document_id("file:///notes.md", &content_hash("doc text"));
        let id1 = chunk_id(&doc_id, "chunk text here", 0, 15);
        let id2 = chunk_id(&doc_id, "chunk text here", 0, 15);
        assert_eq!(id1, id2, "same inputs must produce same chunk ID");
    }

    #[test]
    fn changed_chunk_text_produces_different_chunk_id() {
        let doc_id = document_id("file:///notes.md", &content_hash("doc text"));
        let id1 = chunk_id(&doc_id, "original chunk", 0, 14);
        let id2 = chunk_id(&doc_id, "modified chunk", 0, 14);
        assert_ne!(id1, id2, "changed chunk text must produce different ID");
    }

    #[test]
    fn changed_span_produces_different_chunk_id() {
        let doc_id = document_id("file:///notes.md", &content_hash("doc text"));
        let text = "chunk text";
        let id1 = chunk_id(&doc_id, text, 0, 10);
        let id2 = chunk_id(&doc_id, text, 5, 15); // different span
        assert_ne!(id1, id2, "changed span must produce different chunk ID");
    }

    #[test]
    fn changed_document_id_produces_different_chunk_id() {
        let doc_id1 = document_id("file:///doc1.md", &content_hash("content1"));
        let doc_id2 = document_id("file:///doc2.md", &content_hash("content2"));
        let id1 = chunk_id(&doc_id1, "same text", 0, 9);
        let id2 = chunk_id(&doc_id2, "same text", 0, 9);
        assert_ne!(
            id1, id2,
            "different document must produce different chunk ID"
        );
    }

    #[test]
    fn chunk_id_is_hex_string() {
        let doc_id = document_id("file:///test.md", &content_hash("test"));
        let id = chunk_id(&doc_id, "chunk", 0, 5);
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "chunk ID must be a hex string"
        );
        assert_eq!(id.len(), 64, "blake3 hex output is 64 chars");
    }

    // --- Cross-type stability ---

    #[test]
    fn document_id_stable_across_reruns() {
        // Simulates indexing the same file twice
        let uri = "file:///data/report.md";
        let text = "# Report\n\nSome content here.";
        let hash = content_hash(text);
        let id_run1 = document_id(uri, &hash);
        let id_run2 = document_id(uri, &hash);
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
        let doc_id = document_id(uri, &hash);
        let chunk_text = "Some content here.";
        let id_run1 = chunk_id(&doc_id, chunk_text, 12, 30);
        let id_run2 = chunk_id(&doc_id, chunk_text, 12, 30);
        assert_eq!(id_run1, id_run2, "chunk ID must be stable across re-runs");
    }
}
