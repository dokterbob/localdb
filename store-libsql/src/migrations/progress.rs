//! Progress reporting for `db migrate` (PR #152 comment: a multi-minute
//! migration against a large store produced total silence — no heartbeat, no
//! step indicator). This module defines a purely observational callback
//! vocabulary the migration runner/entry-points emit into; `cli/src/cmds/
//! db.rs` and `cli/src/progress.rs` are the only renderers today.
//!
//! # Contract
//!
//! A [`MigrationProgressSink`] must never influence *what* migrations run,
//! their order, transaction boundaries, or the returned `MigrateReport` — it
//! exists purely so a caller can render a live indicator. Callers that don't
//! want progress reporting pass `None`; every function that accepts a sink
//! has a thin wrapper (`migrate_store`, `apply_pending`) that does exactly
//! that, so existing callers need no changes.

use std::sync::Arc;

/// A lightweight, cheaply-cloneable progress callback for the migration
/// runner. `Send + Sync` so it can be shared into async code without
/// friction, mirroring `localdb_core::progress::ProgressSink`.
pub type MigrationProgressSink = Arc<dyn Fn(MigrationProgressEvent) + Send + Sync>;

/// Progress events emitted during a [`crate::migrate_store_with_progress`]
/// run (or, internally, [`super::runner::apply_pending_with_progress`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationProgressEvent {
    /// Emitted once, before any work starts, on the ordinary incremental
    /// apply-pending path (`BASELINE_VERSION <= current <= head`).
    /// `total_pending` may be `0` (a verified no-op-at-head call still runs
    /// checksum/bookkeeping verification, so this event still fires).
    Started { total_pending: usize },
    /// Emitted once instead of `Started`, for a fresh/0-byte store being
    /// created at head for the first time — this path doesn't step the
    /// chain, so there's no meaningful pending count.
    Initializing,
    /// Emitted once instead of `Started`, for a legacy (pre-baseline)
    /// destructive rebuild — this path doesn't step the chain either.
    RebuildingLegacy,
    /// Emitted immediately before applying pending step `index` (1-based) of
    /// `total`.
    ApplyingStep {
        index: usize,
        total: usize,
        version: i64,
        name: String,
    },
    /// Emitted once, after all work for this run has completed successfully
    /// (including post-migration verification) — the last event of any run.
    Finished,
}
