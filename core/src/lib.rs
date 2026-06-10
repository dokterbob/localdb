//! Core domain model, traits, and shared logic for localdb.
//!
//! This crate contains no I/O frameworks. All domain types, the `RetrievalStore`
//! trait, the `Embedder` trait, and the shared error taxonomy live here.

pub mod error;

/// Re-export key types at the crate root for convenience.
pub use error::Error;
