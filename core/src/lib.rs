//! Core domain model, traits, and shared logic for localdb.
//!
//! This crate contains no I/O frameworks. All domain types, the `RetrievalStore`
//! trait, the `Embedder` trait, the `Ingestor` trait, and the shared error
//! taxonomy live here.

pub mod backend;
pub mod block;
pub mod chunker;
pub mod citation;
pub mod config;
pub mod embedder;
pub mod error;
pub mod heading_index;
pub mod ids;
pub mod ingestion;
pub mod ingestor;
pub mod ingestors;
pub mod markdown_blocks;
pub mod metadata;
pub mod parser;
pub mod progress;
pub mod search;
pub mod source;
pub mod store;
pub mod store_factory;
pub mod types;
pub mod uri;

pub use backend::{
    DocumentInfo, SourceRow, StoreBackend, StoreBackendConfig, StoreBackendConnection, StoreRow,
};
pub use block::{
    Block, BlockKind, BlockLocation, BoundingBox, ChunkLocation, IngestorKind, Resource,
    ResourceKind,
};
pub use chunker::{
    chunk_blocks, CharSizer, ChunkOutput, ChunkSizer, ChunkerConfig, TokenSizer,
};
/// Re-export key types at the crate root for convenience.
pub use citation::Citation;
pub use embedder::{
    DocumentChunks, EmbeddedDocument, Embedder, FakeEmbedder, TokenCounter, VectorEncoding,
};
pub use error::Error;
pub use ids::{chunk_id, content_hash, document_id, new_ulid};
pub use ingestion::{
    complete_index_job, create_index_job, enumerate_path_source, fail_index_job, index_document,
    is_store_stale, run_ingestion_for_source, start_index_job, DocumentExtractor, DocumentIndex,
    DocumentInput, DocumentRecord, ExtractionResult, FetchMetadata, FetchResult, FoundFile,
    IngestionConfig, IngestionResult, UrlFetcher,
};
pub use ingestor::{
    ConfigField, ConfigFieldType, IngestCallback, IngestResult, IngestSource, Ingestor,
    IngestorConfig,
};
pub use ingestors::{FileIngestor, UrlIngestor};
pub use markdown_blocks::{heading_path_from_blocks, markdown_to_blocks};
pub use metadata::{ConversationMetadata, DublinCoreMetadata, Metadata, TranscriptionMetadata};
pub use parser::{ChainParser, DocumentMetadata, ParsedDocument, Parser, Probe, PROBE_HEADER_LEN};
pub use progress::{DocOutcome, ProgressEvent, ProgressSink};
pub use search::{
    rerank_noop, rrf_fuse, rrf_score, shape_citation, FusedChunkEntry, QueryRequest, QueryResponse,
    SearchOrchestrator, StoreHandle,
};
#[cfg(any(test, feature = "test-support"))]
pub use store::FakeStore;
pub use store::{ChunkRecord, MetadataFilter, RetrievalStore, SearchResult, StoreStats};
pub use types::{
    validate_dc_meta_key, validate_msg_meta_key, AclEntry, BackendConfig, Chunk, ChunkingConfig,
    Document, EmbeddingConfig, FederationHop, IndexJob, IndexJobScope, IndexJobState,
    IndexJobStats, IndexingPolicy, Provenance, Source, SourceKind, SourceRef, SourceSpec, Span,
    Store, StoreVisibility,
};
pub use uri::Uri;
