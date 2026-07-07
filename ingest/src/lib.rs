//! Content-acquisition ingestors implementing `core::ingestor::Ingestor`.
//!
//! Issue #117: `core/src/ingestors/{file_ingestor,url_ingestor}.rs` violate
//! the "no I/O in core" invariant (specs/01-architecture.md §1) and are
//! wired into no binary. The *actual* live pipeline is
//! `run_path_source`/`run_url_source` in `core::ingestion`, which has richer
//! behavior (progress hooks, mtime/mime handling, panic tolerance, title
//! merge, conditional-fetch skip/delete semantics). This crate hosts
//! upgraded copies of the two ingestors that bring them to parity with the
//! live pipeline while living outside `core`, as the trait's own doc comment
//! already prescribes.
//!
//! `core`'s originals are left untouched for now; a later wave deletes them
//! once callers are repointed here.

pub mod file_ingestor;
pub mod support;
pub mod url_ingestor;

pub use file_ingestor::FileIngestor;
pub use url_ingestor::UrlIngestor;
