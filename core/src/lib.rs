//! Core domain model, traits, and shared logic for localdb.
//!
//! This crate contains no I/O frameworks. All domain types, the `RetrievalStore`
//! trait, the `Embedder` trait, and the shared error taxonomy live here.

pub mod chunker;
pub mod citation;
pub mod config;
pub mod embedder;
pub mod error;
pub mod ids;
pub mod ingestion;
pub mod parser;
pub mod search;
pub mod store;
pub mod types;

pub use chunker::{chunk_document, ChunkOutput, ChunkerConfig};
/// Re-export key types at the crate root for convenience.
pub use citation::Citation;
pub use embedder::{DocumentChunks, EmbeddedDocument, Embedder, FakeEmbedder};
pub use error::Error;
pub use ids::{chunk_id, content_hash, document_id, new_ulid};
pub use ingestion::{
    complete_index_job, create_index_job, enumerate_path_source, fail_index_job, index_document,
    is_store_stale, run_ingestion_for_source, start_index_job, DocumentExtractor, DocumentIndex,
    DocumentInput, DocumentRecord, ExtractionResult, FetchMetadata, FetchResult, FoundFile,
    IngestionConfig, IngestionResult, UrlFetcher,
};
pub use parser::{ChainParser, DocumentMetadata, ParsedDocument, Parser, Probe, PROBE_HEADER_LEN};
pub use search::{
    rerank_noop, rrf_fuse, rrf_score, shape_citation, FusedChunkEntry, QueryRequest, QueryResponse,
    SearchOrchestrator, StoreHandle,
};
pub use store::{ChunkRecord, FakeStore, MetadataFilter, RetrievalStore, SearchResult, StoreStats};
pub use types::{
    validate_dc_meta_key, validate_msg_meta_key, AclEntry, BackendConfig, Block, BlockKind, Chunk,
    ChunkingConfig, Document, EmbeddingConfig, FederationHop, IndexJob, IndexJobScope,
    IndexJobState, IndexJobStats, IndexingPolicy, Provenance, Source, SourceKind, SourceRef,
    SourceSpec, Span, Store, StoreVisibility,
};
