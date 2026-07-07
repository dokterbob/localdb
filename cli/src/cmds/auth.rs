//! Break-glass auth commands: `localdb user add` and `localdb key create`
//! (specs/05-surfaces.md §2, T3 scope).
//!
//! Both are **embedded/direct-DB only** in T3: they write the unified
//! database's auth tables directly through `core::auth::AuthService`,
//! bypassing HTTP auth entirely — the recovery path when the daemon is
//! unreachable or every admin credential is lost. Whoever can open the
//! database file is already trusted (same boundary as every other
//! daemonless command), so no extra confirmation is required. If a daemon
//! is running they refuse with `daemon_running` (exit 4): SQLite WAL would
//! technically allow the write, but silently mutating auth state underneath
//! a live daemon invites confusion — daemon-routed management (and
//! `--direct-db` lockout recovery) arrives in T5.
//!
//! Remaining subcommands (`user list/remove/set-role`, `key list/revoke`,
//! grants, invites) are T5/T6.

use localdb_core::{
    auth::{AuthStore as _, Role},
    Error,
};
use serde_json::json;

use crate::{
    app_db::{load_app_db, AppDb},
    daemon_client::{probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json},
};

/// Refuse to touch auth tables while a daemon is running (see module doc).
fn refuse_if_daemon_running(ctx: &CliContext, data_dir: &std::path::Path, command: &str) {
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        if !ctx.json {
            eprintln!(
                "A daemon is running at {base_url}. `localdb {command}` writes the database \
                 directly and refuses to run alongside a daemon; stop the daemon \
                 (`localdb serve`) first and retry."
            );
        }
        exit_err(&Error::DaemonRunning, ctx.json);
    }
}

/// `localdb user add <name> [--admin]`
pub fn run_user_add(ctx: &CliContext, name: &str, admin: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_user_add_async(ctx, name, admin));
}

pub(crate) async fn run_user_add_async(ctx: &CliContext, name: &str, admin: bool) {
    if name.trim().is_empty() {
        exit_err(
            &Error::InvalidRequest {
                message: "user name must not be empty".to_string(),
            },
            ctx.json,
        );
    }

    let (config_loader, db) = load_app_db(ctx).await;
    refuse_if_daemon_running(ctx, &config_loader.paths.data_dir, "user add");

    let role = if admin { Role::Admin } else { Role::Member };
    let user = match db.auth_service().create_user(name, role).await {
        Ok(u) => u,
        Err(e) => exit_err(&e, ctx.json),
    };

    let role_str = match user.role {
        Role::Admin => "admin",
        Role::Member => "member",
    };
    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "id": user.id,
            "name": user.name,
            "role": role_str,
        }));
    } else {
        println!(
            "Added user '{}' ({}) with id {}",
            user.name, role_str, user.id
        );
        println!(
            "Create an API key for them with: localdb key create --user {}",
            user.name
        );
    }
}

/// `localdb key create --user <name>`
pub fn run_key_create(ctx: &CliContext, user_name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_key_create_async(ctx, user_name));
}

pub(crate) async fn run_key_create_async(ctx: &CliContext, user_name: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    refuse_if_daemon_running(ctx, &config_loader.paths.data_dir, "key create");

    let issued = match issue_key_for_user(&db, user_name).await {
        Ok(issued) => issued,
        Err(e) => exit_err(&e, ctx.json),
    };

    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "user": user_name,
            "key_id": issued.row.id,
            "secret": issued.secret,
        }));
    } else {
        println!("API key for '{}': {}", user_name, issued.secret);
        println!("Store this now — it is shown only once and cannot be recovered.");
    }
}

/// Look up the user by name and mint an API key (`auth_tokens` row with
/// `kind = 'api_key'`, no expiry, show-once secret).
async fn issue_key_for_user(
    db: &AppDb,
    user_name: &str,
) -> Result<localdb_core::auth::IssuedToken, Error> {
    let user = db
        .auth_store()
        .get_user_by_name(user_name)
        .await?
        .ok_or_else(|| Error::InvalidRequest {
            message: format!(
                "user '{user_name}' does not exist; create it first with \
                 `localdb user add {user_name}`"
            ),
        })?;
    db.auth_service().issue_api_key(&user.id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::auth::{AuthStore, TOKEN_PREFIX};
    use localdb_core::config::loader::ResolvedPaths;
    use localdb_core::config::schema::{EmbeddingPolicy, IndexingPolicyConfig};
    use tempfile::TempDir;

    async fn tmp_app_db(dir: &TempDir) -> AppDb {
        let indexing = IndexingPolicyConfig {
            embedding: EmbeddingPolicy {
                provider: "fake".into(),
                model: "default".into(),
            },
            ..Default::default()
        };
        let paths = ResolvedPaths {
            config_file: dir.path().join("config.yaml"),
            data_dir: dir.path().to_path_buf(),
            models_dir: dir.path().join("models"),
            logs_dir: dir.path().join("logs"),
        };
        AppDb::open(&paths, &indexing.embedding.clone(), &[], indexing)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn issue_key_for_existing_user_mints_show_once_secret() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        db.auth_service()
            .create_user("alice", Role::Admin)
            .await
            .unwrap();

        let issued = issue_key_for_user(&db, "alice").await.unwrap();

        assert!(issued.secret.starts_with(TOKEN_PREFIX));
        // The minted key authenticates against the same database.
        let principal = db
            .auth_service()
            .authenticate(&issued.secret)
            .await
            .unwrap();
        assert_eq!(principal.name, "alice");
        assert_eq!(principal.role, Role::Admin);
    }

    #[tokio::test]
    async fn issue_key_for_unknown_user_is_invalid_request() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;

        let err = issue_key_for_user(&db, "nobody").await.unwrap_err();

        assert!(
            matches!(err, Error::InvalidRequest { ref message } if message.contains("nobody")),
            "expected InvalidRequest naming the user, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn app_db_auth_tables_persist_across_reopen() {
        // The persistence property the daemon relies on: a user created via
        // one AppDb handle (break-glass CLI) is visible when the same
        // database file is opened again (a subsequently started daemon).
        let dir = TempDir::new().unwrap();
        {
            let db = tmp_app_db(&dir).await;
            db.auth_service()
                .create_user("bob", Role::Member)
                .await
                .unwrap();
        }
        let db2 = tmp_app_db(&dir).await;
        let user = db2
            .auth_store()
            .get_user_by_name("bob")
            .await
            .unwrap()
            .expect("user must survive reopen of the on-disk database");
        assert_eq!(user.role, Role::Member);
    }
}
