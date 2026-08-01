//! Schema-migrations framework (issue #127).
//!
//! A migration `chain` is a linear sequence of numbered steps applied on top
//! of a frozen `baseline` schema snapshot. Each [`Migration`] carries both an
//! "up" step (SQL or a Rust callback) and a "down" step, so a database can be
//! moved forward or backward between adjacent versions.
//!
//! This module defines the shared vocabulary (`Migration`, `Up`, `Down`,
//! `RustStep`, `MigrationContext`), the frozen baseline, the (for now empty)
//! real chain, the `schema_migrations` bookkeeping table (`table`), and the
//! checksum machinery that detects drift (`checksum`). `runner` walks the
//! chain forward one step at a time; `migrate` and `downgrade` are the
//! caller-facing entry points `db migrate` / `db downgrade` / `db status`
//! use, each opening its own store via `maintenance::open_for_maintenance`.

pub mod baseline;
pub mod chain;
pub mod checksum;
pub mod downgrade;
pub mod maintenance;
pub mod migrate;
pub mod progress;
pub mod runner;
pub mod table;

#[cfg(test)]
pub(crate) mod test_fixtures;

/// Parameters a migration step needs but can't derive from the connection
/// alone — e.g. the embedding column shape, which is fixed per-store at
/// store-creation time rather than recorded anywhere a plain `PRAGMA` can
/// read it back.
pub struct MigrationContext {
    pub embedding_dim: usize,
    pub encoding: localdb_core::VectorEncoding,
}

/// How a migration's "up" direction is applied.
///
/// `Sql` steps are plain DDL/DML rendered from the context (e.g. to bake in
/// the current embedding dimension) and executed statement-by-statement.
/// `Rust` steps run arbitrary code against the connection for changes SQL
/// alone can't express (e.g. data transformations that need host-language
/// logic).
pub enum Up {
    Sql(fn(&MigrationContext) -> Vec<String>),
    Rust(Box<dyn RustStep>),
}

/// How a migration's "down" direction is applied.
///
/// Down-SQL is rendered once, at apply time, and persisted as data in the
/// `schema_migrations` table (added in a later step) rather than re-derived
/// from the compiled-in [`Migration`] chain. This lets an *older* binary —
/// one that has never heard of this migration — still downgrade past it: it
/// replays the stored SQL strings rather than needing the newer code that
/// produced them.
///
/// Some migrations are not reversible at all (e.g. a step that discards
/// information). Those use `Unsupported` to record why, and any attempt to
/// downgrade past them is refused rather than silently producing a corrupt
/// or lossy database.
pub enum Down {
    Sql(fn(&MigrationContext) -> Vec<String>),
    /// Downgrade past this migration is impossible. The string is a
    /// human-readable reason, stored later as the `down_unsupported_reason`
    /// column in `schema_migrations`.
    Unsupported(&'static str),
}

/// One versioned schema change.
///
/// `version` must be contiguous within a chain (see
/// [`chain::validate_chain`]). `name` and `summary` are for human-readable
/// bookkeeping (migration listings, `schema_migrations` rows); `name` should
/// be a stable identifier (snake_case, no spaces) while `summary` is free text.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub summary: &'static str,
    pub up: Up,
    pub down: Down,
    /// Whether applying this migration invalidates already-derived data
    /// (chunks, embeddings) and should prompt the user to re-run `localdb
    /// index` afterward.
    ///
    /// This is migration "weight class 3" (`specs/02-domain-model.md §9`):
    /// re-embedding/re-extraction work the migration itself can't do (the
    /// embedder/extractors live above `store-libsql`), so the migration
    /// instead bumps `policy_version`/`extractor_version` or truncates
    /// derived rows, and this flag lets `db migrate` print the `localdb
    /// index` hint. `false` for ordinary schema-only changes.
    pub needs_reindex: bool,
}

/// A migration step implemented in Rust rather than plain SQL.
///
/// # Authoring rules
///
/// - A `RustStep` runs inside **one transaction owned by the runner**. It
///   must not begin, commit, or roll back its own transaction — the runner
///   does that around the whole migration (and, where multiple migrations
///   are batched, potentially around several).
/// - Because only DB effects roll back on failure, a step must have **no
///   filesystem or network side effects**. Anything it does must be
///   undoable by the transaction rollback alone.
/// - A step must **not call the ingestion/reindex pipeline** — the embedder
///   and extractors live above `store-libsql` (see `specs/01-architecture.md
///   §1`: no domain logic in surface crates, and conversely no reaching
///   *up* out of this crate into them either). If a schema change makes
///   existing derived data (chunks, embeddings) stale, the step must instead
///   mark that staleness transactionally — e.g. bump `policy_version` /
///   `extractor_version`, truncate the now-invalid derived rows — and let a
///   subsequent `localdb index` do the actual re-embedding/re-extraction
///   work.
#[async_trait::async_trait]
pub trait RustStep: Send + Sync {
    async fn apply(
        &self,
        conn: &libsql::Connection,
        ctx: &MigrationContext,
    ) -> Result<(), libsql::Error>;

    /// A stable, author-provided description of what this step does.
    ///
    /// Used as the "rendered up" input to the row checksum in place of
    /// actual SQL text, since Rust code has no canonical rendering. Authors
    /// must bump this string whenever the step's behavior changes, so the
    /// checksum changes with it and drift is detected.
    fn checksum_repr(&self) -> &'static str;
}
