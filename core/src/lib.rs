//! Core domain model, traits, and shared logic for localdb.
//!
//! This crate contains no I/O frameworks. All domain types, the `RetrievalStore`
//! trait, the `Embedder` trait, the `Ingestor` trait, and the shared error
//! taxonomy live here.

pub mod backend;
pub mod block;
pub mod blocking;
pub mod chunker;
pub mod citation;
pub mod config;
pub mod dates;
pub mod diagnostics;
pub mod documents;
pub mod embedder;
pub mod error;
pub mod heading_index;
pub mod ids;
pub mod ingestion;
pub mod ingestor;
pub mod markdown_blocks;
pub mod metadata;
pub mod parser;
pub mod progress;
pub mod search;
pub mod snippet;
pub mod source;
pub mod store;
pub mod store_factory;
pub mod types;
pub mod uri;

pub use backend::{
    resolve_named_stores, DocumentInfo, SourceRow, StoreBackend, StoreBackendConfig,
    StoreBackendConnection, StoreRow, TableSize,
};
pub use block::{
    Block, BlockKind, BlockLocation, BoundingBox, ChunkLocation, IngestorKind, Resource,
    ResourceKind,
};
pub use blocking::run_blocking;
pub use chunker::{chunk_blocks, CharSizer, ChunkOutput, ChunkSizer, ChunkerConfig, TokenSizer};
/// Re-export key types at the crate root for convenience.
pub use citation::Citation;
pub use diagnostics::{bytes_per_chunk, compute_db_file_size, format_bytes, DbFileSize};
pub use documents::{
    get_document_detail, get_document_detail_scoped, reconstruct_document_text, DocumentDetail,
};
pub use embedder::{
    DocumentChunks, EmbeddedDocument, Embedder, FakeEmbedder, TokenCounter, VectorEncoding,
};
pub use error::Error;
pub use ids::{chunk_id, content_hash, new_ulid, resource_id};
pub use ingestion::{
    complete_index_job, create_index_job, enumerate_path_source, fail_index_job,
    fail_index_job_with_error, index_resource, is_store_stale, run_source_ingestion,
    start_index_job, DeletionPolicy, DocumentIndex, DocumentRecord, FetchMetadata, FetchResult,
    FoundFile, IndexOutcome, IndexResourceDeps, IngestionConfig, IngestionResult, PathEnumeration,
    SourceIngestionDeps, UrlFetcher,
};
pub use ingestor::{
    ConfigField, ConfigFieldType, Enumeration, IngestCallback, IngestResult, IngestSource,
    Ingestor, IngestorConfig,
};
pub use markdown_blocks::{heading_path_from_blocks, markdown_to_blocks};
pub use metadata::{ConversationMetadata, DublinCoreMetadata, Metadata, TranscriptionMetadata};
pub use parser::{ChainParser, ParsedDocument, Parser, Probe, PROBE_HEADER_LEN};
pub use progress::{DocOutcome, ProgressEvent, ProgressSink};
pub use search::{
    clamp_search_limit, rerank_noop, rrf_fuse_global, rrf_score, shape_citation, FusedChunkEntry,
    QueryRequest, QueryResponse, SearchOrchestrator, StoreHandle, SEARCH_MAX_LIMIT,
};
pub use snippet::truncate_snippet;
pub use source::source_row_to_source;
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
