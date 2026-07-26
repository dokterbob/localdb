//! Persistence for dynamically registered OAuth2 clients (`oauth_clients`
//! table, T7, RFC 7591). Mirrors `auth_codes.rs`'s conventions.

use localdb_core::auth::OAuthClientRow;
use localdb_core::Error;

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
    let conn = db.conn().await;
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
    let conn = db.conn().await;
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
