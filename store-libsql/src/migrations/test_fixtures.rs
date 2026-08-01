//! Shared test scaffolding for `migrate.rs` / `downgrade.rs` tests: on-disk
//! fixture builders (baseline DB, baseline + a fixture chain, raw
//! `user_version` stamping) and small reusable fixture chains, plus a
//! before/after DB dump for asserting a refused operation left the store
//! untouched.
//!
//! Extracted here rather than duplicated a third time — `runner.rs` already
//! has its own similarly-shaped fixtures, but those build against an
//! already-open `Connection`, whereas `migrate_store`/`downgrade_store` open
//! their own connection from a `&Path`, so the fixtures here work at the
//! file level instead.
#![cfg(test)]

use std::path::Path;

use libsql::{Builder, Connection};
use tempfile::tempdir;

use super::chain::BASELINE_VERSION;
use super::{baseline, runner, table, Down, Migration, MigrationContext, Up};
use localdb_core::VectorEncoding;

pub(crate) fn ctx() -> MigrationContext {
    MigrationContext {
        embedding_dim: 4,
        encoding: VectorEncoding::Float32,
    }
}

/// Create a 0-byte file at `path` — a valid, empty SQLite database with
/// `user_version == 0`.
pub(crate) fn touch_empty_db_file(path: &Path) {
    std::fs::write(path, []).unwrap();
}

/// Write a raw baseline-only store (no `schema_migrations` table at all) to
/// `path` — what a pre-migrations-framework binary would have left behind.
pub(crate) async fn write_baseline_db(path: &Path) {
    let db = Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
    baseline::create_baseline_schema(&conn, &ctx())
        .await
        .unwrap();
}

/// Write a healthy baseline store to `path` with the `schema_migrations`
/// table present and seeded with just the baseline row (what `LibsqlDb::open`
/// backfills on a healthy at-head store).
pub(crate) async fn write_healthy_baseline_store(path: &Path) {
    let db = Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
    baseline::create_baseline_schema(&conn, &ctx())
        .await
        .unwrap();
    table::ensure_table(&conn).await.unwrap();
    table::ensure_baseline_row(&conn).await.unwrap();
}

/// Write a baseline store to `path`, then apply `chain_migrations` on top via
/// the real runner — leaving rendered `down_sql` rows in `schema_migrations`,
/// exactly as a real upgrade would.
pub(crate) async fn write_baseline_plus_chain(path: &Path, chain_migrations: &[Migration]) {
    let db = Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.query("PRAGMA foreign_keys = ON", ()).await.unwrap();
    baseline::create_baseline_schema(&conn, &ctx())
        .await
        .unwrap();
    runner::apply_pending(&conn, chain_migrations, &ctx())
        .await
        .unwrap();
}

/// Stamp `PRAGMA user_version = version` directly on the store at `path`,
/// bypassing any of this module's normal open paths.
pub(crate) async fn stamp_user_version(path: &Path, version: i64) {
    let db = Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();
    conn.query(&format!("PRAGMA user_version = {version}"), ())
        .await
        .unwrap();
}

/// `sqlite_master` rows with internals (sqlite's own bookkeeping, FTS5
/// shadow tables) and `schema_migrations` itself stripped out, for comparing
/// two databases' user-visible schema.
pub(crate) async fn normalized_master_rows(conn: &Connection) -> Vec<(String, String, String)> {
    let mut rows = conn
        .query(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE 'chunks_fts_%' \
             AND name != 'schema_migrations' \
             ORDER BY type, name",
            (),
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        out.push((
            row.get::<String>(0).unwrap(),
            row.get::<String>(1).unwrap(),
            row.get::<String>(2).unwrap(),
        ));
    }
    out
}

/// Everything about a database file's on-disk state that a refused
/// migrate/downgrade must leave untouched: `sqlite_master`'s DDL rows,
/// `PRAGMA user_version`, and (if present) the `schema_migrations` rows.
/// Mirrors `connection.rs` tests' `DbDump`.
#[derive(Debug, PartialEq)]
pub(crate) struct DbDump {
    pub(crate) master_rows: Vec<(String, String, String)>,
    pub(crate) user_version: i64,
    pub(crate) migration_rows: Vec<table::MigrationRow>,
}

pub(crate) async fn dump_db(path: &Path) -> DbDump {
    let db = Builder::new_local(path).build().await.unwrap();
    let conn = db.connect().unwrap();

    let mut rows = conn
        .query(
            "SELECT type, name, COALESCE(sql, '') FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            (),
        )
        .await
        .unwrap();
    let mut master_rows = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        master_rows.push((
            row.get::<String>(0).unwrap(),
            row.get::<String>(1).unwrap(),
            row.get::<String>(2).unwrap(),
        ));
    }

    let user_version = crate::schema::get_schema_version(&conn).await.unwrap();

    let mut exists = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            (),
        )
        .await
        .unwrap();
    let migration_rows = if exists.next().await.unwrap().is_some() {
        table::list_rows_desc_above(&conn, i64::MIN).await.unwrap()
    } else {
        Vec::new()
    };

    DbDump {
        master_rows,
        user_version,
        migration_rows,
    }
}

// -- Fixture chains -----------------------------------------------------
//
// Two independent 3-step fixture chains (versions BASELINE+1..=BASELINE+3):
// `reversible_chain` where every step has real down-SQL, and
// `chain_with_unsupported_middle` where the middle step is `Down::Unsupported`
// — for exercising downgrade's refuse-cleanly-and-name-the-nearest-target
// path. `chain_with_reindex_marker` is a single step with `needs_reindex:
// true`, for `MigrateReport::staleness_marked`.

fn widgets_up(_ctx: &MigrationContext) -> Vec<String> {
    vec!["CREATE TABLE widgets (id INTEGER PRIMARY KEY, label TEXT)".to_string()]
}
fn widgets_down(_ctx: &MigrationContext) -> Vec<String> {
    vec!["DROP TABLE widgets".to_string()]
}

fn gadgets_up(_ctx: &MigrationContext) -> Vec<String> {
    vec!["CREATE TABLE gadgets (id INTEGER PRIMARY KEY)".to_string()]
}
fn gadgets_down(_ctx: &MigrationContext) -> Vec<String> {
    vec!["DROP TABLE gadgets".to_string()]
}

fn widget_color_up(_ctx: &MigrationContext) -> Vec<String> {
    vec!["ALTER TABLE widgets ADD COLUMN color TEXT".to_string()]
}
fn widget_color_down(_ctx: &MigrationContext) -> Vec<String> {
    vec!["ALTER TABLE widgets DROP COLUMN color".to_string()]
}

pub(crate) fn reversible_chain() -> Vec<Migration> {
    vec![
        Migration {
            version: BASELINE_VERSION + 1,
            name: "add_widgets",
            summary: "fixture: creates the widgets table",
            up: Up::Sql(widgets_up),
            down: Down::Sql(widgets_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 2,
            name: "add_gadgets",
            summary: "fixture: creates the gadgets table",
            up: Up::Sql(gadgets_up),
            down: Down::Sql(gadgets_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 3,
            name: "add_widget_color",
            summary: "fixture: adds widgets.color",
            up: Up::Sql(widget_color_up),
            down: Down::Sql(widget_color_down),
            needs_reindex: false,
        },
    ]
}

fn gizmos_up(_ctx: &MigrationContext) -> Vec<String> {
    vec!["CREATE TABLE gizmos (id INTEGER PRIMARY KEY)".to_string()]
}

pub(crate) fn chain_with_unsupported_middle() -> Vec<Migration> {
    vec![
        Migration {
            version: BASELINE_VERSION + 1,
            name: "add_widgets",
            summary: "fixture: creates the widgets table",
            up: Up::Sql(widgets_up),
            down: Down::Sql(widgets_down),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 2,
            name: "drop_widget_notes_irreversibly",
            summary: "fixture: irreversible add of gizmos",
            up: Up::Sql(gizmos_up),
            down: Down::Unsupported("fixture migration has no down path"),
            needs_reindex: false,
        },
        Migration {
            version: BASELINE_VERSION + 3,
            name: "add_widget_color",
            summary: "fixture: adds widgets.color",
            up: Up::Sql(widget_color_up),
            down: Down::Sql(widget_color_down),
            needs_reindex: false,
        },
    ]
}

fn reindex_marker_up(_ctx: &MigrationContext) -> Vec<String> {
    vec!["CREATE TABLE reindex_marker (id INTEGER PRIMARY KEY)".to_string()]
}
fn reindex_marker_down(_ctx: &MigrationContext) -> Vec<String> {
    vec!["DROP TABLE reindex_marker".to_string()]
}

pub(crate) fn chain_with_reindex_marker() -> Vec<Migration> {
    vec![Migration {
        version: BASELINE_VERSION + 1,
        name: "bump_policy_version",
        summary: "fixture: marks derived data stale",
        up: Up::Sql(reindex_marker_up),
        down: Down::Sql(reindex_marker_down),
        needs_reindex: true,
    }]
}

/// A tempdir + path pair, for tests that just need somewhere to put a store
/// file without caring about the directory's lifetime beyond the test.
pub(crate) fn temp_db_path() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.db");
    (dir, path)
}
