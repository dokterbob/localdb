//! Drift detection: hashing a [`Migration`]'s rendered SQL (or a `RustStep`'s
//! `checksum_repr`) so a stored `schema_migrations` row can be compared
//! against what the currently-compiled chain would produce for that version.
//!
//! A mismatch means the binary's notion of what migration N does has changed
//! since it was applied — e.g. someone edited a shipped migration's SQL in
//! place instead of adding a new one. That's a bug in how migrations are
//! authored (chain entries must be treated as immutable once released, like
//! the `baseline` module), and refusing to proceed is safer than silently
//! running against a database that doesn't match the compiled chain.
//!
//! [`verify_checksums`] does three things, not just one:
//! - **Completeness**: a row must *exist* for the baseline and for every
//!   compiled chain migration up to the caller-supplied `up_to` bound (capped
//!   at this binary's `head_version`) — not just for whatever rows happen to
//!   be present. A store that's missing a row (e.g. a fresh create that died
//!   between `create_schema` stamping `user_version` and the seed rows
//!   landing, or tampering) is corrupt bookkeeping, not an empty-but-valid
//!   history.
//! - **Checksum match**: each present row's `checksum` column must match what
//!   [`migration_checksum`] (or [`baseline_checksum`]) produces today.
//! - **Payload integrity**: each present row's `name`, `down_sql`, and
//!   `down_unsupported_reason` must match what the compiled migration would
//!   render, even if its `checksum` column happens to still read correctly —
//!   catching an edit that touched the payload columns but left `checksum`
//!   untouched (or stale-but-accidentally-equal).

use super::chain::{head_version, BASELINE_VERSION};
use super::table;
use super::{Down, Migration, MigrationContext, Up};

/// Frame a list of rendered SQL statements so that concatenating the frames
/// cannot collide with a different split of the same overall text.
///
/// Each statement is prefixed with its own byte length followed by a NUL
/// separator (`{len}\0{stmt}`), then all frames are concatenated. This is a
/// standard length-prefixed ("netstring"-style) encoding: because each
/// statement's length is recorded immediately before it, the frame
/// boundaries are unambiguous no matter what bytes (including NULs or
/// newlines) the statement itself contains. In particular, a single
/// statement `"A\nB"` frames as `"3\0A\nB"`, while two statements `"A"`,
/// `"B"` frame as `"1\0A1\0B"` — different strings, so they can never hash
/// the same way a naive `.join("\n")` would.
fn frame_statements(statements: &[String]) -> String {
    let mut framed = String::new();
    for stmt in statements {
        framed.push_str(&stmt.len().to_string());
        framed.push('\0');
        framed.push_str(stmt);
    }
    framed
}

/// Blake3 hex digest of a migration's identity plus its rendered up/down
/// steps.
///
/// Input is `version\0name\0<rendered-up>\0<rendered-down-or-reason>`:
/// - rendered-up: `Up::Sql` statements rendered via `ctx`, each length-prefix
///   framed (see [`frame_statements`]) and concatenated; `Up::Rust` uses the
///   step's `checksum_repr()` verbatim.
/// - rendered-down: `Down::Sql` statements rendered via `ctx`, framed the
///   same way; `Down::Unsupported` uses the reason string verbatim.
///
/// Framing each statement by its own length (rather than joining them with a
/// plain separator like `\n`) ensures two different statement splits of the
/// same overall SQL text — e.g. `["A\nB"]` vs. `["A", "B"]` — never produce
/// the same checksum, even though a naive join would render them identically.
pub fn migration_checksum(m: &Migration, ctx: &MigrationContext) -> String {
    let rendered_up = match &m.up {
        Up::Sql(render) => frame_statements(&render(ctx)),
        Up::Rust(step) => step.checksum_repr().to_string(),
    };
    let rendered_down = match &m.down {
        Down::Sql(render) => frame_statements(&render(ctx)),
        Down::Unsupported(reason) => reason.to_string(),
    };
    let input = format!(
        "{version}\0{name}\0{rendered_up}\0{rendered_down}",
        version = m.version,
        name = m.name,
    );
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

/// Blake3 hex digest for the frozen baseline row (`version = BASELINE_VERSION`).
///
/// The baseline predates the migration framework — there's no `Migration`
/// value to render — so this hashes a fixed, arbitrary-but-frozen marker
/// instead. It must never change: doing so would make every existing
/// database's baseline row fail verification.
pub fn baseline_checksum() -> String {
    let input = format!("{BASELINE_VERSION}\0baseline\0<frozen-v4-baseline>");
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

/// Verify every applicable `schema_migrations` row's stored checksum and
/// payload against what the compiled `chain` (rendered with `ctx`) would
/// produce today, AND that no required row is missing.
///
/// `up_to` bounds how far completeness is required to reach — it is capped at
/// `head_version(chain)` internally, so callers may freely pass a database's
/// current `PRAGMA user_version` without checking it against head first.
/// Callers:
/// - `LibsqlDb::open`'s `AtHead` branch passes `head_version(chain)`: the
///   whole chain must be fully and correctly recorded.
/// - `migrate_store` passes the database's current version *before* applying
///   any pending migrations (only the already-applied prefix can possibly
///   have rows yet), then passes `head_version(chain)` again in its
///   post-apply check.
///
/// Checks performed:
/// - **Completeness**: the baseline row (`version == BASELINE_VERSION`) and a
///   row for every chain entry with `version <= min(up_to, head_version(chain))`
///   must exist. A missing row returns `Error::Internal` with correlation id
///   `libsql_migrations_missing_row` naming the missing version.
/// - **Checksum**: every *present* row's `checksum` column, for the baseline
///   and for `BASELINE_VERSION < version <= head_version(chain)`, must match
///   [`baseline_checksum`] / [`migration_checksum`] respectively.
/// - **Payload integrity**: every present chain row's `name`, `down_sql`, and
///   `down_unsupported_reason` must match what the compiled migration renders
///   today — independent of whether its `checksum` column happens to still
///   read correctly, so an edit that touched only the payload columns (and
///   left `checksum` alone) is still caught.
/// - Rows with `version > head_version(chain)` are **skipped** for checksum
///   and payload checks (and don't count toward completeness): they were
///   written by a newer binary than this one, which has already verified
///   them; this (older) binary can still read their stored down-SQL to
///   downgrade past them without understanding what they do.
///
/// Checksum/payload mismatches return `Error::Internal` with correlation id
/// `libsql_migrations_checksum_mismatch` naming the offending migration (and,
/// for payload mismatches, the offending field) on the first mismatch found.
pub async fn verify_checksums(
    conn: &libsql::Connection,
    chain: &[Migration],
    ctx: &MigrationContext,
    up_to: i64,
) -> Result<(), localdb_core::Error> {
    let head = head_version(chain);
    let required_upper = up_to.min(head);
    let rows = table::list_rows_desc_above(conn, BASELINE_VERSION - 1)
        .await
        .map_err(|e| localdb_core::Error::Internal {
            message: format!("reading schema_migrations for checksum verification: {e}"),
            correlation_id: "libsql_migrations_checksum_mismatch".to_string(),
        })?;

    let mut seen_versions = std::collections::HashSet::new();

    for row in &rows {
        seen_versions.insert(row.version);

        if row.version == BASELINE_VERSION {
            let expected = baseline_checksum();
            if row.checksum != expected {
                return Err(mismatch_err(
                    "baseline",
                    row.version,
                    &row.checksum,
                    &expected,
                ));
            }
            continue;
        }

        if row.version > head {
            // Newer than this binary's chain; verified by whichever binary
            // wrote it. Nothing to compare against here.
            continue;
        }

        let Some(migration) = chain.iter().find(|m| m.version == row.version) else {
            // A contiguous, validated chain (see chain::validate_chain) has an
            // entry for every version up to `head`, so this shouldn't happen.
            // Treat it the same as a mismatch rather than silently ignoring
            // a database that's out of sync with the chain.
            return Err(localdb_core::Error::Internal {
                message: format!(
                    "schema_migrations has row for version {v} but no matching chain entry \
                     (head_version={head})",
                    v = row.version,
                ),
                correlation_id: "libsql_migrations_checksum_mismatch".to_string(),
            });
        };

        let expected = migration_checksum(migration, ctx);
        if row.checksum != expected {
            return Err(mismatch_err(
                migration.name,
                row.version,
                &row.checksum,
                &expected,
            ));
        }

        verify_payload(row, migration, ctx)?;
    }

    // Completeness: every version that should have a row up to
    // `required_upper` actually does.
    if !seen_versions.contains(&BASELINE_VERSION) {
        return Err(missing_row_err(BASELINE_VERSION, "baseline"));
    }
    if let Some(migration) = chain
        .iter()
        .find(|m| m.version <= required_upper && !seen_versions.contains(&m.version))
    {
        return Err(missing_row_err(migration.version, migration.name));
    }

    Ok(())
}

/// Compare a present row's payload (`name`, `down_sql`,
/// `down_unsupported_reason`) against what `migration` renders today — see
/// the module docs and [`verify_checksums`] for why this is checked
/// independently of the `checksum` column.
fn verify_payload(
    row: &table::MigrationRow,
    migration: &Migration,
    ctx: &MigrationContext,
) -> Result<(), localdb_core::Error> {
    if row.name != migration.name {
        return Err(payload_mismatch_err(
            migration.name,
            row.version,
            "name",
            &row.name,
            migration.name,
        ));
    }

    match &migration.down {
        Down::Sql(render) => {
            let expected_down = render(ctx);
            if row.down_sql.as_deref() != Some(expected_down.as_slice()) {
                return Err(payload_mismatch_err(
                    migration.name,
                    row.version,
                    "down_sql",
                    &format!("{:?}", row.down_sql),
                    &format!("{expected_down:?}"),
                ));
            }
        }
        Down::Unsupported(reason) => {
            if row.down_unsupported_reason.as_deref() != Some(*reason) {
                return Err(payload_mismatch_err(
                    migration.name,
                    row.version,
                    "down_unsupported_reason",
                    &format!("{:?}", row.down_unsupported_reason),
                    &format!("{reason:?}"),
                ));
            }
        }
    }

    Ok(())
}

fn mismatch_err(name: &str, version: i64, stored: &str, expected: &str) -> localdb_core::Error {
    localdb_core::Error::Internal {
        message: format!(
            "checksum mismatch for migration '{name}' (version {version}): stored={stored}, \
             expected={expected}. The compiled migration's SQL has changed since it was \
             applied to this database."
        ),
        correlation_id: "libsql_migrations_checksum_mismatch".to_string(),
    }
}

fn payload_mismatch_err(
    name: &str,
    version: i64,
    field: &str,
    stored: &str,
    expected: &str,
) -> localdb_core::Error {
    localdb_core::Error::Internal {
        message: format!(
            "stored {field} for migration '{name}' (version {version}) does not match the \
             compiled migration even though its checksum column reads correctly: stored={stored}, \
             expected={expected}. This store's schema_migrations bookkeeping has been tampered \
             with or corrupted."
        ),
        correlation_id: "libsql_migrations_checksum_mismatch".to_string(),
    }
}

fn missing_row_err(version: i64, name: &str) -> localdb_core::Error {
    localdb_core::Error::Internal {
        message: format!(
            "schema_migrations is missing a row for migration '{name}' (version {version}): \
             this store's migration bookkeeping is corrupt or incomplete."
        ),
        correlation_id: "libsql_migrations_missing_row".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::table::MigrationRow;
    use libsql::Builder;
    use localdb_core::{Error, VectorEncoding};
    use tempfile::tempdir;

    fn ctx() -> MigrationContext {
        MigrationContext {
            embedding_dim: 384,
            encoding: VectorEncoding::Float32,
        }
    }

    fn up_a(_ctx: &MigrationContext) -> Vec<String> {
        vec!["CREATE TABLE a(x)".into()]
    }
    fn up_b(_ctx: &MigrationContext) -> Vec<String> {
        vec!["CREATE TABLE b(x)".into()]
    }
    fn down_a(_ctx: &MigrationContext) -> Vec<String> {
        vec!["DROP TABLE a".into()]
    }
    fn down_b(_ctx: &MigrationContext) -> Vec<String> {
        vec!["DROP TABLE b".into()]
    }

    fn base_migration() -> Migration {
        Migration {
            version: 5,
            name: "add_a",
            summary: "adds table a",
            up: Up::Sql(up_a),
            down: Down::Sql(down_a),
            needs_reindex: false,
        }
    }

    #[test]
    fn checksum_changes_when_version_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.version = 6;
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_name_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.name = "add_a_renamed";
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_up_sql_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.up = Up::Sql(up_b);
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_down_sql_changes() {
        let c = ctx();
        let m1 = base_migration();
        let mut m2 = base_migration();
        m2.down = Down::Sql(down_b);
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn checksum_changes_when_down_becomes_unsupported_with_different_reasons() {
        let c = ctx();
        let mut m1 = base_migration();
        m1.down = Down::Unsupported("reason one");
        let mut m2 = base_migration();
        m2.down = Down::Unsupported("reason two");
        assert_ne!(migration_checksum(&m1, &c), migration_checksum(&m2, &c));
    }

    #[test]
    fn baseline_checksum_is_deterministic() {
        assert_eq!(baseline_checksum(), baseline_checksum());
    }

    // C2: `Up::Sql`/`Down::Sql` render to `Vec<String>` (one entry per
    // statement), but naively joining with `\n` before hashing means a
    // migration rendered as `["A\nB"]` (one statement containing a literal
    // newline) and one rendered as `["A", "B"]` (two separate statements)
    // hash identically — both join to the same `"A\nB"` string. That would
    // let a shipped migration's statement boundaries be silently
    // split/merged (changing runtime behavior — e.g. how errors roll back,
    // or what `replay_one`/the runner execute as separate `tx.execute`
    // calls) while its checksum still verifies. The checksum must be over a
    // structured representation that can't collide across statement
    // boundaries.

    #[test]
    fn checksum_does_not_collide_across_up_sql_statement_boundaries() {
        let c = ctx();

        fn up_one_joined_statement(_ctx: &MigrationContext) -> Vec<String> {
            vec!["A\nB".to_string()]
        }
        fn up_two_separate_statements(_ctx: &MigrationContext) -> Vec<String> {
            vec!["A".to_string(), "B".to_string()]
        }

        let mut joined = base_migration();
        joined.up = Up::Sql(up_one_joined_statement);
        let mut split = base_migration();
        split.up = Up::Sql(up_two_separate_statements);

        assert_ne!(
            migration_checksum(&joined, &c),
            migration_checksum(&split, &c),
            "a single statement containing '\\n' must not hash the same as two \
             statements joined by '\\n'"
        );
    }

    #[test]
    fn checksum_does_not_collide_across_down_sql_statement_boundaries() {
        let c = ctx();

        fn down_one_joined_statement(_ctx: &MigrationContext) -> Vec<String> {
            vec!["A\nB".to_string()]
        }
        fn down_two_separate_statements(_ctx: &MigrationContext) -> Vec<String> {
            vec!["A".to_string(), "B".to_string()]
        }

        let mut joined = base_migration();
        joined.down = Down::Sql(down_one_joined_statement);
        let mut split = base_migration();
        split.down = Down::Sql(down_two_separate_statements);

        assert_ne!(
            migration_checksum(&joined, &c),
            migration_checksum(&split, &c),
            "a single statement containing '\\n' must not hash the same as two \
             statements joined by '\\n'"
        );
    }

    async fn open_test_db() -> (tempfile::TempDir, libsql::Connection) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Builder::new_local(&path).build().await.unwrap();
        let conn = db.connect().unwrap();
        (dir, conn)
    }

    /// A fixture migration with `Down::Unsupported`, at version 5
    /// (`BASELINE_VERSION + 1`) — for tests that use it as the sole entry of
    /// a single-migration chain.
    fn unsupported_migration() -> Migration {
        Migration {
            version: 5,
            name: "add_a_unsupported",
            summary: "adds table a, irreversibly",
            up: Up::Sql(up_a),
            down: Down::Unsupported("original reason: a cannot be dropped safely"),
            needs_reindex: false,
        }
    }

    /// A second chain entry, at version 6 (`BASELINE_VERSION + 2`), for tests
    /// that pair it with [`base_migration`] in a 2-entry chain.
    fn second_chain_migration() -> Migration {
        Migration {
            version: 6,
            name: "add_b_unsupported",
            summary: "adds table b, irreversibly",
            up: Up::Sql(up_b),
            down: Down::Unsupported("original reason: b cannot be dropped safely"),
            needs_reindex: false,
        }
    }

    async fn insert_matching_row(
        conn: &libsql::Connection,
        migration: &Migration,
        ctx: &MigrationContext,
    ) {
        let (down_sql, down_unsupported_reason) = match &migration.down {
            Down::Sql(render) => (Some(render(ctx)), None),
            Down::Unsupported(reason) => (None, Some(reason.to_string())),
        };
        table::insert_row(
            conn,
            &MigrationRow {
                version: migration.version,
                name: migration.name.to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql,
                down_unsupported_reason,
                checksum: migration_checksum(migration, ctx),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn verify_checksums_passes_on_freshly_built_matching_table() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = base_migration();
        insert_matching_row(&conn, &migration, &c).await;

        let chain = vec![migration];
        verify_checksums(&conn, &chain, &c, BASELINE_VERSION + 1)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn verify_checksums_passes_when_down_is_unsupported_and_untampered() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = unsupported_migration();
        insert_matching_row(&conn, &migration, &c).await;

        let chain = vec![migration];
        verify_checksums(&conn, &chain, &c, BASELINE_VERSION + 1)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn verify_checksums_fails_when_a_row_checksum_is_corrupted() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = base_migration();
        insert_matching_row(&conn, &migration, &c).await;

        conn.execute(
            "UPDATE schema_migrations SET checksum = 'tampered' WHERE version = ?",
            libsql::params![migration.version],
        )
        .await
        .unwrap();

        let chain = vec![migration];
        let err = verify_checksums(&conn, &chain, &c, BASELINE_VERSION + 1)
            .await
            .expect_err("tampered checksum should fail verification");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_checksum_mismatch");
                assert!(
                    message.contains("add_a"),
                    "message should name migration: {message}"
                );
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_checksums_ignores_rows_newer_than_head_version() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        // No entries in the compiled chain, so head_version == BASELINE_VERSION.
        let chain: Vec<Migration> = Vec::new();
        let c = ctx();

        // A row from a newer binary this one has never heard of, with a
        // checksum that would never match anything we could compute.
        table::insert_row(
            &conn,
            &MigrationRow {
                version: BASELINE_VERSION + 1,
                name: "from_the_future".to_string(),
                applied_at: "2024-06-01T00:00:00Z".to_string(),
                down_sql: Some(vec!["DROP TABLE future_thing".to_string()]),
                down_unsupported_reason: None,
                checksum: "nonsense-checksum".to_string(),
            },
        )
        .await
        .unwrap();

        verify_checksums(&conn, &chain, &c, BASELINE_VERSION)
            .await
            .expect("rows above head_version should be skipped, not fail verification");
    }

    // -- Finding 1: completeness — a row must exist for every compiled chain
    // migration up to `up_to`, not just for whatever happens to be present.

    #[tokio::test]
    async fn verify_checksums_fails_when_baseline_row_is_missing() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();
        table::delete_row(&conn, BASELINE_VERSION).await.unwrap();

        let c = ctx();
        let chain: Vec<Migration> = Vec::new();
        let err = verify_checksums(&conn, &chain, &c, BASELINE_VERSION)
            .await
            .expect_err("missing baseline row must fail verification");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_missing_row");
                assert!(
                    message.contains("baseline"),
                    "message should name the baseline row: {message}"
                );
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_checksums_fails_when_a_chain_row_is_missing() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let m1 = base_migration();
        let m2 = second_chain_migration();
        insert_matching_row(&conn, &m1, &c).await;
        insert_matching_row(&conn, &m2, &c).await;

        // Simulate a store that died (or was tampered with) between stamping
        // user_version at head and finishing the bookkeeping insert: the row
        // for the second chain entry is gone even though the schema itself
        // (not exercised here) may already be at head.
        table::delete_row(&conn, m2.version).await.unwrap();

        let chain = vec![m1, m2];
        let head = head_version(&chain);
        let err = verify_checksums(&conn, &chain, &c, head)
            .await
            .expect_err("missing chain row must fail verification");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_missing_row");
                assert!(
                    message.contains("add_b_unsupported"),
                    "message should name the missing migration: {message}"
                );
                assert!(
                    message.contains('6'),
                    "message should name the missing version: {message}"
                );
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_checksums_up_to_bounds_completeness_to_the_applied_prefix() {
        // A "pending" store: only the first of a 2-step chain has been
        // applied (and recorded) so far. Bounding `up_to` at the applied
        // version must NOT demand a row for the not-yet-applied second
        // entry.
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let m1 = base_migration();
        let m2 = second_chain_migration();
        insert_matching_row(&conn, &m1, &c).await;
        // m2 deliberately not inserted — it hasn't been applied yet.

        let up_to = m1.version;
        let chain = vec![m1, m2];
        verify_checksums(&conn, &chain, &c, up_to)
            .await
            .expect("completeness must only be required up to `up_to`");
    }

    // -- Finding 2: payload integrity — a tampered `name`/`down_sql`/
    // `down_unsupported_reason` must be caught even when the `checksum`
    // column itself was left alone.

    #[tokio::test]
    async fn verify_checksums_fails_when_name_is_tampered_but_checksum_is_intact() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = base_migration();
        insert_matching_row(&conn, &migration, &c).await;

        conn.execute(
            "UPDATE schema_migrations SET name = 'tampered_name' WHERE version = ?",
            libsql::params![migration.version],
        )
        .await
        .unwrap();

        let chain = vec![migration];
        let err = verify_checksums(&conn, &chain, &c, BASELINE_VERSION + 1)
            .await
            .expect_err("tampered name must fail verification even with an intact checksum");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_checksum_mismatch");
                assert!(message.contains("name"), "message: {message}");
                assert!(message.contains("tampered_name"), "message: {message}");
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_checksums_fails_when_down_sql_is_tampered_but_checksum_is_intact() {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = base_migration();
        insert_matching_row(&conn, &migration, &c).await;

        conn.execute(
            "UPDATE schema_migrations SET down_sql = '[\"DROP TABLE bogus\"]' WHERE version = ?",
            libsql::params![migration.version],
        )
        .await
        .unwrap();

        let chain = vec![migration];
        let err = verify_checksums(&conn, &chain, &c, BASELINE_VERSION + 1)
            .await
            .expect_err("tampered down_sql must fail verification even with an intact checksum");
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_checksum_mismatch");
                assert!(message.contains("down_sql"), "message: {message}");
                assert!(message.contains("bogus"), "message: {message}");
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_checksums_fails_when_down_unsupported_reason_is_tampered_but_checksum_is_intact(
    ) {
        let (_dir, conn) = open_test_db().await;
        table::ensure_table(&conn).await.unwrap();
        table::ensure_baseline_row(&conn).await.unwrap();

        let c = ctx();
        let migration = unsupported_migration();
        insert_matching_row(&conn, &migration, &c).await;

        conn.execute(
            "UPDATE schema_migrations SET down_unsupported_reason = 'tampered reason' \
             WHERE version = ?",
            libsql::params![migration.version],
        )
        .await
        .unwrap();

        let chain = vec![migration];
        let err = verify_checksums(&conn, &chain, &c, BASELINE_VERSION + 1)
            .await
            .expect_err(
                "tampered down_unsupported_reason must fail verification even with an intact \
                 checksum",
            );
        match err {
            Error::Internal {
                message,
                correlation_id,
            } => {
                assert_eq!(correlation_id, "libsql_migrations_checksum_mismatch");
                assert!(
                    message.contains("down_unsupported_reason"),
                    "message: {message}"
                );
                assert!(message.contains("tampered reason"), "message: {message}");
            }
            other => panic!("expected Error::Internal, got {other:?}"),
        }
    }
}
