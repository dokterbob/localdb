//! Concrete ingestor implementations.
//!
//! These implement the [`Ingestor`] trait from `crate::ingestor` for file
//! and URL sources.  They live in `core` as the primary implementations used
//! by the CLI; more complex I/O-framework-heavy ingestors (Notion, Telegram,
//! …) will live in separate crates.

pub mod file_ingestor;
pub mod url_ingestor;

pub use file_ingestor::FileIngestor;
pub use url_ingestor::UrlIngestor;
