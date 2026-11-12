//! Persistence for dynamically registered OAuth2 clients (`oauth_clients`
//! table, RFC 7591). Mirrors `auth_codes.rs`'s conventions.

use localdb_core::auth::{OAuthClientRow, RegisterClientOutcome};
use localdb_core::Error;

use super::atomic::finish;
use crate::connection::{map_libsql_err, LibsqlDb};

const CLIENT_COLUMNS: &str = "id, client_name, redirect_uris, created_at";

pub(crate) async fn create_oauth_client(
    db: &LibsqlDb,
    client: &OAuthClientRow,
) -> Result<(), Error> {
    let redirect_uris_json =
        serde_json::to_string(&client.redirect_uris).map_err(|e| Error::Internal {
            message: format!("failed to serialize oauth_clients.redirect_uris: {e}"),
            correlation_id: "oauth_client_redirect_uris_serialize".to_string(),
        })?;
    let conn = db.writer().await;
    conn.execute(
        "INSERT INTO oauth_clients (id, client_name, redirect_uris, created_at)
         VALUES (?, ?, ?, ?)",
        libsql::params![
            client.id.clone(),
            client.client_name.clone(),
            redirect_uris_json,
            client.created_at.clone(),
        ],
    )
    .await
    .map_err(map_libsql_err)?;
    Ok(())
}

pub(crate) async fn find_oauth_client(
    db: &LibsqlDb,
    id: &str,
) -> Result<Option<OAuthClientRow>, Error> {
    let conn = db.reader();
    let mut rows = conn
        .query(
            &format!("SELECT {CLIENT_COLUMNS} FROM oauth_clients WHERE id = ?"),
            libsql::params![id.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;
    match rows.next().await.map_err(map_libsql_err)? {
        Some(row) => row_to_oauth_client(&row).map(Some),
        None => Ok(None),
    }
}

/// Atomic prune/evict-then-insert for `POST /register` (specs/05-surfaces.md
/// §3.1). The `DELETE`s and the cap-check `COUNT` run inside the same write
/// transaction as the eventual `INSERT`, so two concurrent registrations can
/// never both observe room under `max_dynamic_clients` and both insert.
///
/// "Used" is derived from the existing `auth_codes` table rather than a new
/// column: a client that has ever had an authorization code issued to it
/// (`auth_codes.client_id`) has completed the earliest step of the OAuth2
/// flow this schema records, strictly before any token is minted from that
/// code — the earliest reliable signal available without a schema change.
///
/// If the TTL prune above leaves the table still at the cap, this evicts
/// the single oldest unused dynamic client (ties broken by `id`) rather than
/// refusing outright: an unauthenticated caller that fills every slot with
/// fresh registrations must not be able to lock out every other
/// registration attempt until those rows age past `unused_before` on their
/// own. A client that registered but has not yet started authorization can
/// be evicted this way and must register again. Only when every remaining
/// slot is held by a client with a completed authorization (or the built-in
/// client, which never counts) does this refuse with
/// [`RegisterClientOutcome::CapReached`].
pub(crate) async fn prune_and_create_oauth_client(
    db: &LibsqlDb,
    client: &OAuthClientRow,
    builtin_client_id: &str,
    unused_before: &str,
    max_dynamic_clients: u64,
) -> Result<RegisterClientOutcome, Error> {
    let tx = db.write_tx().await?;
    let result = async {
        tx.execute(
            "DELETE FROM oauth_clients \
             WHERE id != ? AND created_at < ? \
               AND NOT EXISTS (\
                 SELECT 1 FROM auth_codes WHERE auth_codes.client_id = oauth_clients.id\
               )",
            libsql::params![builtin_client_id.to_string(), unused_before.to_string()],
        )
        .await
        .map_err(map_libsql_err)?;

        let mut rows = tx
            .query(
                "SELECT COUNT(*) FROM oauth_clients WHERE id != ?",
                libsql::params![builtin_client_id.to_string()],
            )
            .await
            .map_err(map_libsql_err)?;
        let dynamic_count: i64 = match rows.next().await.map_err(map_libsql_err)? {
            Some(row) => row.get(0).map_err(map_libsql_err)?,
            None => 0,
        };
        if dynamic_count as u64 >= max_dynamic_clients {
            // Evict the oldest unused dynamic client to free exactly the
            // one slot this registration needs. The subquery's `id = NULL`
            // result when no unused row exists deletes nothing, so
            // `evicted` distinguishes that case from an actual eviction.
            let evicted = tx
                .execute(
                    "DELETE FROM oauth_clients WHERE id = (\
                       SELECT id FROM oauth_clients \
                       WHERE id != ? \
                         AND NOT EXISTS (\
                           SELECT 1 FROM auth_codes WHERE auth_codes.client_id = oauth_clients.id\
                         ) \
                       ORDER BY created_at ASC, id ASC \
                       LIMIT 1\
                     )",
                    libsql::params![builtin_client_id.to_string()],
                )
                .await
                .map_err(map_libsql_err)?;
            if evicted == 0 {
                // Still commits: whatever the TTL prune above removed stays
                // pruned even though nothing is inserted this time.
                return Ok(RegisterClientOutcome::CapReached);
            }
        }

        let redirect_uris_json =
            serde_json::to_string(&client.redirect_uris).map_err(|e| Error::Internal {
                message: format!("failed to serialize oauth_clients.redirect_uris: {e}"),
                correlation_id: "oauth_client_redirect_uris_serialize".to_string(),
            })?;
        tx.execute(
            "INSERT INTO oauth_clients (id, client_name, redirect_uris, created_at) \
             VALUES (?, ?, ?, ?)",
            libsql::params![
                client.id.clone(),
                client.client_name.clone(),
                redirect_uris_json,
                client.created_at.clone(),
            ],
        )
        .await
        .map_err(map_libsql_err)?;
        Ok(RegisterClientOutcome::Created)
    }
    .await;
    finish(tx, result).await
}

fn row_to_oauth_client(row: &libsql::Row) -> Result<OAuthClientRow, Error> {
    let id: String = row.get(0).map_err(map_libsql_err)?;
    let client_name: Option<String> = row.get(1).map_err(map_libsql_err)?;
    let redirect_uris_json: String = row.get(2).map_err(map_libsql_err)?;
    let created_at: String = row.get(3).map_err(map_libsql_err)?;
    let redirect_uris: Vec<String> =
        serde_json::from_str(&redirect_uris_json).map_err(|e| Error::Internal {
            message: format!("invalid oauth_clients.redirect_uris JSON: {e}"),
            correlation_id: "oauth_client_redirect_uris_parse".to_string(),
        })?;
    Ok(OAuthClientRow {
        id,
        client_name,
        redirect_uris,
        created_at,
    })
}
