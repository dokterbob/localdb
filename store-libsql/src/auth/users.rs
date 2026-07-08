use localdb_core::auth::{Role, UserRow};
use localdb_core::Error;

use super::sql::{role_to_sql, row_to_user};
use crate::connection::{map_libsql_err, LibsqlDb};

pub(crate) async fn create_user(db: &LibsqlDb, user: &UserRow) -> Result<(), Error> {
    let conn = db.conn().await;
    conn.execute(
        "INSERT INTO users (id, name, role, created_at) VALUES (?, ?, ?, ?)",
        libsql::params![
            user.id.clone(),
            user.name.clone(),
            role_to_sql(user.role).to_string(),
            user.created_at.clone(),
        ],
    )
    .await
    .map_err(|e| {
        // A UNIQUE violation on `name` means the caller lost a race (or
        // skipped the pre-check `AuthService::create_user` normally does) —
        // surface it as InvalidRequest rather than a generic Internal error.
        let msg = e.to_string();
        if msg.contains("UNIQUE") {
            Error::InvalidRequest {
                message: format!("user '{}' already exists", user.name),
            }
        } else {
            map_libsql_err(e)
        }
    })?;
    Ok(())
}

pub(crate) async fn get_user(db: &LibsqlDb, id: &str) -> Result<Option<UserRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, name, role, created_at FROM users WHERE id = ?",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_user(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn get_user_by_name(db: &LibsqlDb, name: &str) -> Result<Option<UserRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, name, role, created_at FROM users WHERE name = ?",
            libsql::params![name.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_user(&row).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn list_users(db: &LibsqlDb) -> Result<Vec<UserRow>, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT id, name, role, created_at FROM users ORDER BY created_at",
            (),
        )
        .await
        .map_err(map_libsql_err)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_libsql_err)? {
        out.push(row_to_user(&row)?);
    }
    Ok(out)
}

pub(crate) async fn update_user_role(db: &LibsqlDb, id: &str, role: Role) -> Result<(), Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "UPDATE users SET role = ? WHERE id = ?",
            libsql::params![role_to_sql(role).to_string(), id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    if n == 0 {
        return Err(Error::InvalidRequest {
            message: format!("user '{id}' not found"),
        });
    }
    Ok(())
}

pub(crate) async fn delete_user(db: &LibsqlDb, id: &str) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "DELETE FROM users WHERE id = ?",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

/// Atomic "delete iff this wouldn't drop admins to zero" (D7 last-admin
/// lockout guard, finding #5): the eligibility check — whether `id` is
/// currently the *sole* admin — is folded into the `DELETE`'s own WHERE
/// clause via a correlated subquery, so two concurrent guarded deletes (or a
/// guarded delete racing `try_demote_user_unless_last_admin`) against the
/// same two-admin instance can never both succeed and leave zero admins.
/// Returns `true` iff this call deleted the row.
pub(crate) async fn try_delete_user_unless_last_admin(
    db: &LibsqlDb,
    id: &str,
) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "DELETE FROM users WHERE id = ? AND ( \
                 role <> 'admin' \
                 OR (SELECT COUNT(*) FROM users WHERE role = 'admin') > 1 \
             )",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

/// Atomic "demote to member iff this wouldn't drop admins to zero" — the
/// demotion counterpart to `try_delete_user_unless_last_admin`, same
/// correlated-subquery-in-the-WHERE-clause convention. Returns `true` iff
/// this call performed the demotion; `false` covers "unknown id", "already
/// not an admin", and "would have left zero admins".
pub(crate) async fn try_demote_user_unless_last_admin(
    db: &LibsqlDb,
    id: &str,
) -> Result<bool, Error> {
    let conn = db.conn().await;
    let n = conn
        .execute(
            "UPDATE users SET role = 'member' WHERE id = ? AND role = 'admin' \
             AND (SELECT COUNT(*) FROM users WHERE role = 'admin') > 1",
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    Ok(n > 0)
}

pub(crate) async fn count_users(db: &LibsqlDb) -> Result<u64, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query("SELECT COUNT(*) FROM users", ())
        .await
        .map_err(map_libsql_err)?;
    let row = rows
        .next()
        .await
        .map_err(map_libsql_err)?
        .ok_or_else(|| Error::Internal {
            message: "COUNT(*) returned no rows".to_string(),
            correlation_id: "auth_count_users".to_string(),
        })?;
    let count: i64 = row.get(0).map_err(map_libsql_err)?;
    Ok(count as u64)
}

/// Whether at least one `Role::Admin` user exists (finding #5) — backs the
/// setup-code bootstrap decision, which must key off admin existence rather
/// than mere user existence.
pub(crate) async fn admin_exists(db: &LibsqlDb) -> Result<bool, Error> {
    let conn = db.conn().await;
    let mut rows = conn
        .query(
            "SELECT EXISTS(SELECT 1 FROM users WHERE role = 'admin')",
            (),
        )
        .await
        .map_err(map_libsql_err)?;
    let row = rows
        .next()
        .await
        .map_err(map_libsql_err)?
        .ok_or_else(|| Error::Internal {
            message: "EXISTS(...) returned no rows".to_string(),
            correlation_id: "auth_admin_exists".to_string(),
        })?;
    let exists: i64 = row.get(0).map_err(map_libsql_err)?;
    Ok(exists != 0)
}
