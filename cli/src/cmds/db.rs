//! `localdb db status` / `db migrate` / `db downgrade` — schema-migration
//! maintenance commands (specs/05-surfaces.md §2.1).
//!
//! These are the only surfaces allowed to touch a store's schema version.
//! Unlike every other command in this crate, they must resolve `(path,
//! MigrationContext)` from config alone — never through `AppDb::open`, which
//! refuses on the very version mismatch these commands exist to fix, and
//! never by constructing an embedder, which can trigger a large model
//! download just to answer "what version is this store at". See
//! `app_db::load_config_for_maintenance`'s doc comment.

use std::path::Path;

use localdb_core::{config::loader::ConfigLoader, Error, VectorEncoding};
use serde_json::json;
use store_libsql::{
    downgrade_store, inspect_schema, migrate_store_with_progress, vacuum_store, MigrateReport,
    MigrationContext, SchemaStatus,
};

use crate::{
    app_db::{load_config_for_maintenance, reject_store_flag},
    daemon_client::{probe_daemon, CliContext, DaemonState},
    normalize::{confirm_destructive, exit_err, print_json},
};

/// Refuse with `Error::DaemonRunning` (exit 4) if a daemon is up.
///
/// Per specs/05-surfaces.md §2.1: `db status`/`db migrate`/`db downgrade`
/// are CLI-only and require the daemon to be stopped — unlike `store`/
/// `source`/`index`/`search`, they never route to the daemon's HTTP API,
/// because the daemon itself never applies migrations.
fn refuse_if_daemon_running(ctx: &CliContext, config_loader: &ConfigLoader) {
    if let DaemonState::Running { .. } =
        probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref())
    {
        exit_err(&Error::DaemonRunning, ctx.json);
    }
}

/// Statically derive a `MigrationContext` from config — see the module doc
/// comment and `app_db::load_config_for_maintenance` for why this must not
/// construct an embedder.
fn migration_context_from_config(config_loader: &ConfigLoader) -> Result<MigrationContext, Error> {
    let (embedding_dim, encoding) = embed::infer_dim_encoding(
        &config_loader.config.defaults.indexing.embedding,
        &config_loader.config.providers,
    )
    .map_err(|e| Error::InvalidConfig {
        message: format!("cannot determine embedding shape: {e}"),
    })?;
    Ok(MigrationContext {
        embedding_dim,
        encoding,
    })
}

/// Whether `db migrate`'s completion summary should point the user at
/// `localdb db vacuum`.
///
/// Today that's exactly the case where the `shrink_vector_index` migration
/// (v6, `store-libsql/src/migrations/chain.rs`) actually applied *and*
/// rebuilt the index — its `up` step is a real `DROP INDEX`/`CREATE INDEX`
/// (freeing pages onto SQLite's free list) only on `VectorEncoding::Binary`
/// stores; on `Float32` stores it's bookkeeping-only and frees nothing, so
/// the hint would be actively misleading there. Named by migration, not by a
/// generic "this migration frees pages" flag on `Migration` — introduce that
/// generalization if/when a second page-freeing migration lands, rather than
/// speculatively building it for a chain of exactly one.
///
/// Pulled out as a pure function of `MigrateReport` so it's unit-testable
/// without a real database.
/// The schema version whose up-step rebuilds the DiskANN index.
const SHRINK_VECTOR_INDEX_VERSION: i64 = 6;

/// Whether `db migrate` is about to run the v6 index rebuild on this store.
///
/// Pure predicate over the pre-inspection, so it's unit-testable without a
/// database. `pre.legacy` stores are excluded: a legacy rebuild recreates the
/// schema from scratch at head rather than stepping the chain, so it never
/// runs v6's up-step and never leaves a bloated old index behind.
fn index_shrink_pending(pre: &SchemaStatus, encoding: VectorEncoding) -> bool {
    encoding == VectorEncoding::Binary
        && !pre.legacy
        && pre.current_version < SHRINK_VECTOR_INDEX_VERSION
        && pre.head_version >= SHRINK_VECTOR_INDEX_VERSION
}

/// Warn, before any work starts, that this migration will not shrink the file
/// — it will briefly *grow* it.
///
/// The v6 rebuild writes a new, ~9x smaller index while the old one's pages go
/// to the free list rather than back to the filesystem, so peak on-disk size
/// during and after the migration exceeds the starting size until `db vacuum`
/// runs. That ordering surprises people (issue #177 is someone running
/// `VACUUM` *before* anything had been freed and concluding it did nothing),
/// and the users most likely to hit it are disk-constrained by definition —
/// they're here because a store grew to tens of GB. Reporting the current size
/// up front lets them judge headroom before committing to a long operation,
/// rather than discovering it partway through.
///
/// stderr in both human and `--json` modes, so `--json` stdout stays clean.
fn warn_if_index_shrink_pending(pre: &SchemaStatus, mctx: &MigrationContext, path: &Path) {
    if !index_shrink_pending(pre, mctx.encoding) {
        return;
    }
    let current = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "note: this migration rebuilds the vector index (~9x smaller) by re-reading the \
         stored embeddings — no re-embedding, but it does one index insert per chunk and \
         can take a long time on a large store."
    );
    eprintln!(
        "      it does NOT shrink the file: the space it frees goes to SQLite's free list, \
         so '{}' ({}) will briefly grow before `localdb db vacuum` reclaims it.",
        path.display(),
        format_bytes(current),
    );
}

fn vacuum_recommended(report: &MigrateReport, encoding: VectorEncoding) -> bool {
    encoding == VectorEncoding::Binary
        && report
            .applied
            .iter()
            .any(|step| step.name == "shrink_vector_index")
}

/// Pending-migration count for `db status`: the number of chain entries
/// between a healthy store's current version and this binary's head.
///
/// Zero for a legacy store (below baseline — that's a rebuild, not a
/// pending-apply count), zero for a store at or beyond head (nothing
/// pending; "beyond head" is the too-new case, reported separately), and
/// zero for an uninitialized store (`current_version == 0` — a store file
/// that exists but has no schema at all, e.g. a zero-byte file). The
/// uninitialized case is reported distinctly by `print_status` via its own
/// `uninitialized` flag rather than as a `pending` gap: there's nothing to
/// incrementally *apply* to a store with no schema yet, only a fresh create
/// (`db migrate`, or any normal command).
///
/// Pulled out as a pure function of `SchemaStatus` so it's unit-testable
/// without a real database.
fn pending_count(status: &SchemaStatus) -> i64 {
    if status.current_version >= status.baseline_version
        && status.current_version < status.head_version
    {
        status.head_version - status.current_version
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// db status
// ---------------------------------------------------------------------------

/// `localdb db status`
pub fn run_db_status(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_db_status_async(ctx));
}

pub(crate) async fn run_db_status_async(ctx: &CliContext) {
    reject_store_flag(ctx);
    let config_loader = load_config_for_maintenance(ctx);
    refuse_if_daemon_running(ctx, &config_loader);

    let path = config_loader.paths.db_path();
    let status = match inspect_schema(&path).await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    print_status(ctx, &status);
}

fn print_status(ctx: &CliContext, status: &SchemaStatus) {
    let pending = pending_count(status);
    let too_new = status.current_version > status.head_version;
    // An existing-but-uninitialized store: the file/connection opens fine,
    // but `PRAGMA user_version` is still 0 — no schema has ever been
    // created (the maintenance path explicitly supports this, e.g. a
    // zero-byte file the user pointed at). This must not fall through to
    // "up to date" just because `pending == 0`: there's no schema to be
    // "up to date" *with*.
    let uninitialized = status.current_version == 0;

    if ctx.json {
        let rows: Vec<serde_json::Value> = status
            .rows
            .iter()
            .map(|r| {
                json!({
                    "version": r.version,
                    "name": r.name,
                    "applied_at": r.applied_at,
                    "downgradable": r.down_sql.is_some(),
                    "down_unsupported_reason": r.down_unsupported_reason,
                })
            })
            .collect();
        print_json(&json!({
            "current_version": status.current_version,
            "head_version": status.head_version,
            "baseline_version": status.baseline_version,
            // Deliberately left at 0 (not `head_version - 0`) for an
            // uninitialized store: `pending` counts incremental steps
            // available to apply on top of an existing schema, and an
            // uninitialized store has no schema to apply on top of — it
            // needs a fresh create, not a migration chain. Callers must
            // check `uninitialized` first; a `pending == 0` alongside
            // `uninitialized == true` means "needs init", not "up to date".
            "pending": pending,
            "legacy": status.legacy,
            "too_new": too_new,
            "uninitialized": uninitialized,
            "table_present": status.table_present,
            "migrations": rows,
        }));
        return;
    }

    println!(
        "schema version: {} (this binary's head: {}, baseline: {})",
        status.current_version, status.head_version, status.baseline_version
    );
    if uninitialized {
        println!(
            "store exists but is uninitialized (no schema yet); any normal localdb command, or \
             `localdb db migrate`, will initialize it to v{}",
            status.head_version
        );
    } else if status.legacy {
        println!(
            "legacy store: predates the migration framework (v{}); run `localdb db migrate` \
             to rebuild it — destructive, all indexed data is lost, then re-run `localdb index`",
            status.current_version
        );
    } else if too_new {
        println!(
            "store is newer than this binary (v{} > v{}); run `localdb db downgrade` with this \
             binary to step it back, or upgrade localdb",
            status.current_version, status.head_version
        );
    } else if pending > 0 {
        println!(
            "{pending} pending migration{s}; run `localdb db migrate`",
            s = if pending == 1 { "" } else { "s" }
        );
    } else {
        println!("up to date");
    }

    if status.rows.is_empty() {
        println!("no migration history (schema_migrations table not present)");
    } else {
        println!("history:");
        for row in &status.rows {
            let downgrade_info = match &row.down_unsupported_reason {
                Some(reason) => format!("not downgradable: {reason}"),
                None => "downgradable".to_string(),
            };
            println!(
                "  v{} {}  applied {}  ({downgrade_info})",
                row.version, row.name, row.applied_at
            );
        }
    }
}

// ---------------------------------------------------------------------------
// db migrate
// ---------------------------------------------------------------------------

/// `localdb db migrate`
pub fn run_db_migrate(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_db_migrate_async(ctx));
}

pub(crate) async fn run_db_migrate_async(ctx: &CliContext) {
    reject_store_flag(ctx);
    let config_loader = load_config_for_maintenance(ctx);
    refuse_if_daemon_running(ctx, &config_loader);

    let path = config_loader.paths.db_path();
    let mctx = match migration_context_from_config(&config_loader) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    };

    // Pre-inspect (read-only) only to decide whether the legacy-rebuild
    // confirmation prompt is needed — `migrate_store` itself never prompts;
    // the CLI's confirmation is what actually authorizes a legacy rebuild's
    // data loss. This must NOT be used to short-circuit an "already at head"
    // report: `migrate_store`'s own no-op-at-head path still runs full
    // checksum/bookkeeping verification (`post_check`), and skipping the
    // library call here would let `db migrate` report false success on an
    // at-head store with corrupted migration bookkeeping. Every other
    // command refuses to open a store in that state; this is the one
    // command meant to fix/diagnose it, so it must always go through
    // `migrate_store`.
    let pre = match inspect_schema(&path).await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    let allow_legacy_rebuild = if pre.legacy {
        let prompt = format!(
            "This store's schema (v{}) predates the migration baseline (v{}); migrating it \
             erases ALL indexed data and rebuilds from scratch. Continue?",
            pre.current_version, pre.baseline_version,
        );
        if !confirm_destructive(ctx, &prompt) {
            // Standard aborted-by-user path (mirrors `store remove`, etc.):
            // `confirm_destructive` already printed "Aborted." to stderr;
            // just return, leaving the store untouched, exit 0.
            return;
        }
        true
    } else {
        false
    };

    warn_if_index_shrink_pending(&pre, &mctx, &path);

    // `None` in `--json` mode (stdout must stay clean JSON) or when stderr
    // isn't a terminal, `build_migration_progress_sink` still returns
    // `Some` for the piped case (bounded plain lines) — see its doc comment.
    // This is what closes the "total silence during minutes of disk I/O"
    // gap from PR #152's report: a live heartbeat spinner (or, piped, a
    // bounded set of step lines) now renders while `migrate_store_with_progress`
    // runs, instead of nothing until the whole call returns.
    let progress_sink = crate::progress::build_migration_progress_sink(ctx.json);
    let report = match migrate_store_with_progress(
        &path,
        &mctx,
        allow_legacy_rebuild,
        progress_sink,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => exit_err(&e, ctx.json),
    };

    // A true no-op: nothing applied and the version didn't move (this can
    // only happen via the incremental at-head path, never the fresh-create
    // or legacy-rebuild paths, both of which always change from_version).
    // `migrate_store` still ran `post_check`'s checksum/bookkeeping
    // verification to get here, so this is a verified "already at head",
    // not merely an unexamined pre-inspect snapshot.
    let noop_at_head = !report.legacy_rebuilt
        && report.applied.is_empty()
        && report.from_version == report.to_version;

    if noop_at_head {
        if ctx.json {
            print_json(&json!({
                "status": "ok",
                "message": format!("already at head (v{})", report.to_version),
            }));
        } else {
            println!("already at head (v{})", report.to_version);
        }
        return;
    }

    let vacuum_hint = vacuum_recommended(&report, mctx.encoding);

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "from_version": report.from_version,
            "to_version": report.to_version,
            "steps": report.applied.len(),
            "legacy_rebuilt": report.legacy_rebuilt,
            "staleness_marked": report.staleness_marked,
            "vacuum_recommended": vacuum_hint,
        }));
        return;
    }

    if report.legacy_rebuilt {
        println!(
            "rebuilt legacy store: v{} -> v{} (all indexed data erased)",
            report.from_version, report.to_version
        );
    } else {
        println!(
            "migrated: v{} -> v{} ({} step{} applied)",
            report.from_version,
            report.to_version,
            report.applied.len(),
            if report.applied.len() == 1 { "" } else { "s" }
        );
    }
    if report.staleness_marked {
        println!("hint: run `localdb index` to re-index stale content");
    }
    if vacuum_hint {
        println!(
            "hint: this migration shrank the vector index but freed pages stay in the file \
             until reclaimed — run `localdb db vacuum` to shrink it on disk"
        );
    }
}

// ---------------------------------------------------------------------------
// db downgrade
// ---------------------------------------------------------------------------

/// Pre-validate a resolved downgrade target against `status`, mirroring
/// `downgrade_store`'s own up-front checks (same order, same wording) —
/// before `run_db_downgrade_async` asks for destructive confirmation.
///
/// An impossible downgrade (already at or below the frozen baseline, a
/// target at/above the current version, or an irreversible migration in
/// `(target, current_version]` — one whose `down_unsupported_reason` is
/// set) can only ever fail once it reaches `downgrade_store`, so demanding a
/// "yes, I'm sure" answer — or, non-interactively, the generic "re-run with
/// --yes" refusal — for it is misleading: it implies the operation is
/// destructive-but-possible, not simply invalid. Checking here lets the CLI
/// surface the real error instead, without ever prompting.
///
/// This is a shortcut, not a replacement: `downgrade_store` remains the
/// authority and re-validates independently against the live store, so a
/// TOCTOU race (the store changing between this check and the actual call)
/// still fails safely there.
fn validate_downgrade_target(status: &SchemaStatus, target: i64) -> Result<(), Error> {
    if target < status.baseline_version {
        return Err(Error::InvalidConfig {
            message: format!(
                "cannot downgrade below the frozen baseline version {}: the baseline schema \
                 predates the migration framework and has no down-SQL to replay",
                status.baseline_version
            ),
        });
    }
    if target >= status.current_version {
        return Err(Error::InvalidConfig {
            message: format!(
                "nothing to downgrade: target version {target} must be below the current \
                 version {}",
                status.current_version
            ),
        });
    }

    // Mirror `downgrade_store`'s own pre-scan for `down_unsupported_reason`
    // rows in `(target, current_version]` — same range, same message. An
    // irreversible migration in that range means `downgrade_store` will
    // refuse no matter what, so surface its real error here too, before
    // `run_db_downgrade_async` asks for destructive confirmation for an
    // operation that was never possible. When more than one blocking row
    // falls in range, name the highest-versioned one (the nearest reachable
    // target), matching `downgrade_store`'s own selection.
    if let Some(blocked) = status
        .rows
        .iter()
        .filter(|r| r.version > target && r.version <= status.current_version)
        .filter(|r| r.down_unsupported_reason.is_some())
        .max_by_key(|r| r.version)
    {
        let reason = blocked.down_unsupported_reason.as_deref().unwrap_or("");
        return Err(Error::InvalidConfig {
            message: format!(
                "cannot downgrade past migration '{name}' (version {version}): {reason}. \
                 Nothing was changed. Downgrade to version {version} instead (`db downgrade \
                 --to {version}`) to keep it applied and only replay the migrations above it.",
                name = blocked.name,
                version = blocked.version,
            ),
        });
    }

    Ok(())
}

/// `localdb db downgrade [--to N]`
///
/// Defaults to one step back when `--to` is not given. `downgrade_store`'s
/// own `target: None` default is the frozen baseline instead — a sensible
/// default for a library call, but not what specs/05-surfaces.md §2
/// documents for the CLI ("default: one step"). Rather than changing the
/// library's default (a program calling it directly might reasonably want
/// "reset to baseline"), the CLI always resolves an explicit target itself,
/// so it never actually exercises the library's own `None` branch.
pub fn run_db_downgrade(ctx: &CliContext, to: Option<i64>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_db_downgrade_async(ctx, to));
}

pub(crate) async fn run_db_downgrade_async(ctx: &CliContext, to: Option<i64>) {
    reject_store_flag(ctx);
    let config_loader = load_config_for_maintenance(ctx);
    refuse_if_daemon_running(ctx, &config_loader);

    let path = config_loader.paths.db_path();

    // Always inspect first — both to resolve the CLI's own "one step back"
    // default and to pre-validate the target before the destructive-
    // confirmation prompt below (see `validate_downgrade_target`).
    let status = match inspect_schema(&path).await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };
    let target = to.unwrap_or(status.current_version - 1);

    if let Err(e) = validate_downgrade_target(&status, target) {
        exit_err(&e, ctx.json);
    }

    let prompt = format!(
        "This reverses the store's schema to version {target}, replaying stored down-SQL and \
         discarding any data or structure introduced by later migrations. Continue?"
    );
    if !confirm_destructive(ctx, &prompt) {
        return;
    }

    let report = match downgrade_store(&path, Some(target)).await {
        Ok(r) => r,
        Err(e) => exit_err(&e, ctx.json),
    };

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "from_version": report.from_version,
            "to_version": report.to_version,
            "steps": report.steps.len(),
        }));
    } else {
        println!(
            "downgraded: v{} -> v{} ({} step{})",
            report.from_version,
            report.to_version,
            report.steps.len(),
            if report.steps.len() == 1 { "" } else { "s" }
        );
    }
}

// ---------------------------------------------------------------------------
// db vacuum
// ---------------------------------------------------------------------------

/// Render a byte count as a human-readable size (binary units). Small enough
/// not to warrant a dependency for it.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `localdb db vacuum`
pub fn run_db_vacuum(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_db_vacuum_async(ctx));
}

pub(crate) async fn run_db_vacuum_async(ctx: &CliContext) {
    reject_store_flag(ctx);
    let config_loader = load_config_for_maintenance(ctx);
    refuse_if_daemon_running(ctx, &config_loader);

    let path = config_loader.paths.db_path();

    // VACUUM is data-safe — SQLite builds a full replacement file and swaps
    // it in atomically, so an interrupted run leaves the original untouched
    // — unlike the legacy-rebuild/downgrade paths above, which lose data and
    // gate behind `confirm_destructive`. It's resource-heavy instead: warn
    // rather than prompt. Printed to stderr in both human and `--json` modes
    // so `--json` stdout stays clean.
    eprintln!(
        "vacuuming '{}': this rewrites the entire database file and needs roughly its current \
         size again in free disk space; large stores can take minutes",
        path.display()
    );

    let report = match vacuum_store(&path).await {
        Ok(r) => r,
        Err(e) => exit_err(&e, ctx.json),
    };

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "size_before_bytes": report.size_before,
            "size_after_bytes": report.size_after,
            "bytes_reclaimed": report.bytes_reclaimed,
            "duration_ms": report.duration.as_millis() as u64,
        }));
        return;
    }

    println!(
        "vacuumed: {} -> {} ({} reclaimed, {:.1}s)",
        format_bytes(report.size_before),
        format_bytes(report.size_after),
        format_bytes(report.bytes_reclaimed),
        report.duration.as_secs_f64(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        current: i64,
        head: i64,
        baseline: i64,
        legacy: bool,
        table_present: bool,
    ) -> SchemaStatus {
        SchemaStatus {
            current_version: current,
            head_version: head,
            baseline_version: baseline,
            rows: Vec::new(),
            legacy,
            table_present,
        }
    }

    #[test]
    fn pending_count_is_zero_when_at_head() {
        let s = status(4, 4, 4, false, true);
        assert_eq!(pending_count(&s), 0);
    }

    #[test]
    fn pending_count_reports_gap_between_current_and_head() {
        let s = status(4, 7, 4, false, true);
        assert_eq!(pending_count(&s), 3);
    }

    #[test]
    fn pending_count_is_zero_for_legacy_store_below_baseline() {
        // A legacy (v1-v3) store needs a rebuild, not an incremental apply —
        // pending_count must not report a (current..head) gap for it.
        let s = status(2, 4, 4, true, false);
        assert_eq!(pending_count(&s), 0);
    }

    #[test]
    fn pending_count_is_zero_when_store_is_newer_than_head() {
        let s = status(9, 4, 4, false, true);
        assert_eq!(pending_count(&s), 0);
    }

    // Codex review #152 fix 1: an uninitialized store (current_version == 0,
    // e.g. an existing zero-byte file) must not report a `head - 0` pending
    // count — `print_status` reports it via its own `uninitialized` flag
    // instead, so `pending_count` staying 0 here matters as the honest
    // building block for that.
    #[test]
    fn pending_count_is_zero_for_uninitialized_store() {
        let s = status(0, 4, 4, false, false);
        assert_eq!(pending_count(&s), 0);
    }

    fn downgrade_status(current: i64, baseline: i64) -> SchemaStatus {
        status(current, current, baseline, false, true)
    }

    #[test]
    fn validate_downgrade_target_rejects_target_below_baseline() {
        let s = downgrade_status(6, 4);
        let err = validate_downgrade_target(&s, 3).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("cannot downgrade below the frozen baseline version 4"),
            "message: {message}"
        );
    }

    #[test]
    fn validate_downgrade_target_rejects_target_at_or_above_current() {
        let s = downgrade_status(4, 4);
        let err = validate_downgrade_target(&s, 4).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("nothing to downgrade"),
            "message: {message}"
        );
    }

    #[test]
    fn validate_downgrade_target_accepts_a_plausible_target() {
        let s = downgrade_status(7, 4);
        assert!(validate_downgrade_target(&s, 5).is_ok());
    }

    fn unsupported_row(
        version: i64,
        name: &str,
        reason: &str,
    ) -> store_libsql::migrations::table::MigrationRow {
        store_libsql::migrations::table::MigrationRow {
            version,
            name: name.to_string(),
            applied_at: "2026-01-01T00:00:00Z".to_string(),
            down_sql: None,
            down_unsupported_reason: Some(reason.to_string()),
            checksum: "deadbeef".to_string(),
        }
    }

    // C6: an unsupported (irreversible) migration inside (target,
    // current_version] must fail *before* `run_db_downgrade_async` ever asks
    // for destructive confirmation — `downgrade_store` would refuse anyway,
    // but only after the misleading prompt. See `validate_downgrade_target`'s
    // doc comment.
    #[test]
    fn validate_downgrade_target_rejects_target_below_unsupported_migration_in_range() {
        let mut s = downgrade_status(7, 4);
        s.rows = vec![unsupported_row(
            6,
            "widen_embeddings",
            "column widened irreversibly",
        )];

        let err = validate_downgrade_target(&s, 5).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("cannot downgrade past migration 'widen_embeddings' (version 6)"),
            "message: {message}"
        );
        assert!(
            message.contains("column widened irreversibly"),
            "message: {message}"
        );
        assert!(
            message.contains("Nothing was changed."),
            "message: {message}"
        );
        assert!(
            message.contains("db downgrade --to 6"),
            "message: {message}"
        );
    }

    #[test]
    fn validate_downgrade_target_ignores_unsupported_migration_above_current_version() {
        // A row above current_version can never be in the replay range, so
        // it must be irrelevant even though `down_unsupported_reason` is set.
        let mut s = downgrade_status(7, 4);
        s.rows = vec![unsupported_row(9, "future_migration", "n/a")];

        assert!(validate_downgrade_target(&s, 5).is_ok());
    }

    #[test]
    fn validate_downgrade_target_ignores_unsupported_migration_at_or_below_target() {
        // A row at or below the requested target is being kept applied, not
        // replayed, so it must not block the downgrade.
        let mut s = downgrade_status(7, 4);
        s.rows = vec![unsupported_row(5, "kept_migration", "n/a")];

        assert!(validate_downgrade_target(&s, 5).is_ok());
    }

    // -- vacuum_recommended / format_bytes --------------------------------

    fn applied_step(name: &str) -> store_libsql::migrations::runner::AppliedStep {
        store_libsql::migrations::runner::AppliedStep {
            version: 6,
            name: name.to_string(),
            duration: std::time::Duration::from_millis(1),
        }
    }

    fn migrate_report(
        applied: Vec<store_libsql::migrations::runner::AppliedStep>,
    ) -> MigrateReport {
        MigrateReport {
            from_version: 5,
            to_version: 6,
            applied,
            legacy_rebuilt: false,
            staleness_marked: false,
        }
    }

    #[test]
    fn index_shrink_pending_true_for_binary_store_below_v6() {
        assert!(index_shrink_pending(
            &status(5, 6, 4, false, true),
            VectorEncoding::Binary
        ));
    }

    #[test]
    fn index_shrink_pending_false_for_float32_store() {
        // v6's up-step renders zero statements for Float32 — nothing is
        // rebuilt, nothing is freed, so the warning would be a lie.
        assert!(!index_shrink_pending(
            &status(5, 6, 4, false, true),
            VectorEncoding::Float32
        ));
    }

    #[test]
    fn index_shrink_pending_false_when_already_at_or_past_v6() {
        assert!(!index_shrink_pending(
            &status(6, 6, 4, false, true),
            VectorEncoding::Binary
        ));
    }

    #[test]
    fn index_shrink_pending_false_for_legacy_rebuild() {
        // A legacy store is rebuilt from scratch at head rather than stepped
        // through the chain, so v6's up-step never runs and no bloated index
        // is left behind to reclaim.
        assert!(!index_shrink_pending(
            &status(2, 6, 4, true, true),
            VectorEncoding::Binary
        ));
    }

    #[test]
    fn index_shrink_pending_false_when_binary_head_predates_v6() {
        // An older binary whose compiled chain stops before v6 must not
        // promise a rebuild it cannot perform.
        assert!(!index_shrink_pending(
            &status(5, 5, 4, false, true),
            VectorEncoding::Binary
        ));
    }

    #[test]
    fn vacuum_recommended_true_when_shrink_vector_index_applied_on_binary_store() {
        let report = migrate_report(vec![applied_step("shrink_vector_index")]);
        assert!(vacuum_recommended(&report, VectorEncoding::Binary));
    }

    #[test]
    fn vacuum_recommended_false_on_float32_store_even_if_migration_applied() {
        // v6's up-step is a no-op on Float32 stores (already correctly
        // tuned) — nothing was freed, so no hint should fire.
        let report = migrate_report(vec![applied_step("shrink_vector_index")]);
        assert!(!vacuum_recommended(&report, VectorEncoding::Float32));
    }

    #[test]
    fn vacuum_recommended_false_when_migration_did_not_apply() {
        let report = migrate_report(vec![applied_step("some_other_migration")]);
        assert!(!vacuum_recommended(&report, VectorEncoding::Binary));
    }

    #[test]
    fn vacuum_recommended_false_when_nothing_applied() {
        let report = migrate_report(vec![]);
        assert!(!vacuum_recommended(&report, VectorEncoding::Binary));
    }

    #[test]
    fn vacuum_recommended_false_for_legacy_rebuild() {
        // legacy_rebuilt never populates `applied` (it recreates the schema
        // from scratch rather than stepping the chain), so this is covered
        // by the "nothing applied" case above, but assert the shape
        // explicitly since it's the one real-world path where a full rebuild
        // (which *does* free plenty of pages) still shouldn't get this hint
        // — `staleness_marked`'s reindex hint already covers that case.
        let mut report = migrate_report(vec![]);
        report.legacy_rebuilt = true;
        assert!(!vacuum_recommended(&report, VectorEncoding::Binary));
    }

    #[test]
    fn format_bytes_formats_across_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }
}
