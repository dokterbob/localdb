//! Content-acquisition ingestors implementing `core::ingestor::Ingestor`.
//!
//! Issue #117: concrete ingestors must live outside `core` to respect the
//! "no I/O in core" invariant (specs/01-architecture.md §1) — the trait and
//! the pipeline that drives it (`core::ingestion::run_source_ingestion`,
//! `index_resource`) stay in `core`, but acquisition I/O (filesystem reads,
//! HTTP clients) lives here. `FileIngestor` and `UrlIngestor` are the CLI's
//! concrete ingestors for path and URL sources, with progress hooks,
//! mtime/mime handling, panic tolerance, title merge, and conditional-fetch
//! skip/delete semantics.

pub mod feed_ingestor;
pub mod file_ingestor;
pub mod support;
pub mod url_ingestor;
pub(crate) mod url_pipeline;

pub use feed_ingestor::FeedIngestor;
pub use file_ingestor::FileIngestor;
pub use url_ingestor::UrlIngestor;
