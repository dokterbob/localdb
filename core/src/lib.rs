//! Core domain model, traits, and shared logic for localdb.
//!
//! This crate contains no I/O frameworks. All domain types, the `RetrievalStore`
//! trait, the `Embedder` trait, and the shared error taxonomy live here.

pub mod citation;
pub mod embedder;
pub mod error;
pub mod ids;
pub mod search;
pub mod store;
pub mod types;

/// Re-export key types at the crate root for convenience.
pub use citation::Citation;
pub use embedder::{DocumentChunks, EmbeddedDocument, Embedder, FakeEmbedder};
pub use error::Error;
pub use ids::{chunk_id, content_hash, document_id, new_ulid};
pub use search::{FusedResult, QueryRequest, QueryResponse, SearchOrchestrator, StoreHandle};
pub use store::{ChunkRecord, FakeStore, MetadataFilter, RetrievalStore, SearchResult, StoreStats};
pub use types::{
    validate_msg_meta_key, AclEntry, BackendConfig, Block, BlockKind, Chunk, ChunkingConfig,
    Document, EmbeddingConfig, FederationHop, IndexJob, IndexJobScope, IndexJobState,
    IndexJobStats, IndexingPolicy, Provenance, Source, SourceKind, SourceRef, SourceSpec, Span,
    Store, StoreVisibility,
};
