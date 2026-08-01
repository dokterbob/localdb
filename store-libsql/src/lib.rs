mod backend;
mod connection;
pub mod migrations;
mod registry;
mod schema;
mod tenant;
mod vectors;

pub use backend::SqliteBackend;

// Maintenance API for the CLI's `db migrate` / `db downgrade` / `db status`
// commands (specs/05-surfaces.md §2.1) — these are the only surfaces allowed
// to touch a store's schema version; the HTTP daemon and MCP only ever
// surface the open-time refusal-with-hint (see `connection.rs`).
pub use migrations::chain::{head_version_current, BASELINE_VERSION};
pub use migrations::downgrade::{
    downgrade_store, inspect_schema, DowngradeReport, DowngradeStep, SchemaStatus,
};
pub use migrations::migrate::{migrate_store, migrate_store_with_progress, MigrateReport};
pub use migrations::progress::{MigrationProgressEvent, MigrationProgressSink};
pub use migrations::MigrationContext;
