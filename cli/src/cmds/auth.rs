//! Auth management commands: `localdb user add|list|remove|set-role` and
//! `localdb key create|list|revoke` (specs/05-surfaces.md §2).
//!
//! **Every** command in this module follows the same
//! **daemon-routed-with-direct-DB-fallback** pattern `cli/src/cmds/store.rs`
//! already established for `store add|list|remove`: when a daemon is
//! reachable, the request goes over HTTP with the caller's bearer (admin
//! where the route requires it — `Principal::require_admin` on the server
//! side maps a member's attempt to `forbidden`/exit 6); otherwise it falls
//! back to a direct, trusted database write/read (embedded mode's existing
//! "whoever can open the database file is already trusted" boundary). This
//! makes "add a user while the server is up" just work over HTTP, the
//! common case, rather than requiring the daemon to be stopped first.
//!
//! `user add` and `key create` additionally accept `--direct-db`, which
//! forces the direct-DB path even while a daemon is running — the
//! lockout-recovery escape hatch for when the daemon's own auth is broken
//! or every admin credential has been lost (specs/05-surfaces.md §2). This
//! is the one deliberate exception to "prefer the daemon when it's up":
//! `--direct-db` warns (non-JSON mode) that a daemon is running and
//! proceeds anyway rather than refusing — SQLite's WAL mode plus
//! `busy_timeout` already make concurrent access with a live daemon safe
//! (worst case, the write waits and then times out to `RuntimeStateLocked`,
//! exit 4, the same as any other contended direct-DB write), so there is no
//! correctness reason to hard-refuse here, only a possible race the warning
//! makes visible.
//!
//! Grants and invites (beyond `store grant|revoke`, in `cli/src/cmds/store.rs`)
//! are T6.

use localdb_core::{
    auth::{AuthStore as _, Role, UserRow},
    Error,
};
use serde_json::json;

use crate::{
    app_db::{load_app_db, AppDb},
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json},
};

/// Render `Role` as the wire string used by both the HTTP API and the CLI's
/// own JSON output ("admin" | "member").
fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

/// Parse a CLI-supplied role string, rejecting anything but "admin"/"member".
fn parse_role_arg(ctx: &CliContext, s: &str) -> Role {
    match s {
        "admin" => Role::Admin,
        "member" => Role::Member,
        other => exit_err(
            &Error::InvalidRequest {
                message: format!("unknown role '{other}'; expected 'admin' or 'member'"),
            },
            ctx.json,
        ),
    }
}

fn user_row_json(u: &UserRow) -> serde_json::Value {
    json!({
        "id": u.id,
        "name": u.name,
        "role": role_to_str(u.role),
        "created_at": u.created_at,
    })
}

fn print_user_list(ctx: &CliContext, users: &[serde_json::Value]) {
    if ctx.json {
        print_json(&json!({ "users": users }));
    } else if users.is_empty() {
        println!("No users.");
    } else {
        for u in users {
            println!(
                "{} [{}] {}",
                u["name"].as_str().unwrap_or("?"),
                u["role"].as_str().unwrap_or("?"),
                u["id"].as_str().unwrap_or("?"),
            );
        }
    }
}

/// Non-fatal warning for the `--direct-db` escape hatch (see module doc):
/// unlike the rest of this module, `--direct-db` deliberately proceeds with
/// a direct database write even when a daemon is running, so this only
/// warns rather than refusing.
fn warn_if_daemon_running_direct_db(ctx: &CliContext, data_dir: &std::path::Path, command: &str) {
    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        if !ctx.json {
            eprintln!(
                "Warning: a daemon is running at {base_url}. `localdb {command} --direct-db` \
                 writes the database directly anyway (lockout recovery) — this may briefly \
                 contend with the daemon's own writes, but SQLite's write-ahead log and busy \
                 timeout make it safe."
            );
        }
    }
}

/// `localdb user add <name> [--admin] [--direct-db]`
pub fn run_user_add(ctx: &CliContext, name: &str, admin: bool, direct_db: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_user_add_async(ctx, name, admin, direct_db));
}

pub(crate) async fn run_user_add_async(ctx: &CliContext, name: &str, admin: bool, direct_db: bool) {
    if name.trim().is_empty() {
        exit_err(
            &Error::InvalidRequest {
                message: "user name must not be empty".to_string(),
            },
            ctx.json,
        );
    }
    let role = if admin { Role::Admin } else { Role::Member };

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if direct_db {
        warn_if_daemon_running_direct_db(ctx, data_dir, "user add");
    } else if let DaemonState::Running { base_url } =
        probe_daemon(data_dir, ctx.daemon_url.as_deref())
    {
        let url = format!("{base_url}/v1/users");
        let body = json!({ "name": name, "role": role_to_str(role) });
        match daemon_request_async(ctx, reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!(
                        "Added user '{}' ({}) with id {} (via daemon)",
                        v.get("name").and_then(|n| n.as_str()).unwrap_or(name),
                        v.get("role")
                            .and_then(|r| r.as_str())
                            .unwrap_or(role_to_str(role)),
                        v.get("id").and_then(|i| i.as_str()).unwrap_or("?"),
                    );
                    println!("Create an API key for them with: localdb key create --user {name}");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let user = match db.auth_service().create_user(name, role).await {
        Ok(u) => u,
        Err(e) => exit_err(&e, ctx.json),
    };

    let role_str = role_to_str(user.role);
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

/// `localdb key create --user <name> [--direct-db]`
pub fn run_key_create(ctx: &CliContext, user_name: &str, direct_db: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_key_create_async(ctx, user_name, direct_db));
}

pub(crate) async fn run_key_create_async(ctx: &CliContext, user_name: &str, direct_db: bool) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if direct_db {
        warn_if_daemon_running_direct_db(ctx, data_dir, "key create");
    } else if let DaemonState::Running { base_url } =
        probe_daemon(data_dir, ctx.daemon_url.as_deref())
    {
        let id = resolve_user_id_for_key_create(ctx, &base_url, user_name).await;
        let url = format!("{base_url}/v1/users/{id}/keys");
        match daemon_request_async(ctx, reqwest::Method::POST, &url, None).await {
            Ok(v) => {
                let secret = v.get("secret").and_then(|s| s.as_str()).unwrap_or("");
                if ctx.json {
                    print_json(&json!({
                        "status": "ok",
                        "user": user_name,
                        "key_id": v.get("id"),
                        "secret": secret,
                    }));
                } else {
                    println!("API key for '{user_name}': {secret} (via daemon)");
                    println!("Store this now — it is shown only once and cannot be recovered.");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

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

/// Resolve the target user's ID for a daemon-routed `key create`, without
/// requiring an admin-only `GET /v1/users` list when the caller is minting
/// a key for *themselves* (self-service is allowed for any role,
/// specs/05-surfaces.md §3.1). `GET /v1/auth/me` is answerable by any
/// authenticated principal; only the "for someone else" branch needs (and
/// implicitly requires, via the server's own admin check) an admin bearer.
async fn resolve_user_id_for_key_create(
    ctx: &CliContext,
    base_url: &str,
    user_name: &str,
) -> String {
    let me_url = format!("{base_url}/v1/auth/me");
    if let Ok(me) = daemon_request_async(ctx, reqwest::Method::GET, &me_url, None).await {
        if me.get("name").and_then(|n| n.as_str()) == Some(user_name) {
            if let Some(id) = me.get("user_id").and_then(|i| i.as_str()) {
                return id.to_string();
            }
        }
    }
    resolve_user_id_via_daemon(ctx, base_url, user_name).await
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

// ---------------------------------------------------------------------------
// T5: daemon-routed-with-direct-DB-fallback commands
// ---------------------------------------------------------------------------

/// `localdb user list`
pub fn run_user_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_user_list_async(ctx));
}

pub(crate) async fn run_user_list_async(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/users");
        match daemon_request_async(ctx, reqwest::Method::GET, &url, None).await {
            Ok(v) => {
                let users = v.as_array().cloned().unwrap_or_default();
                print_user_list(ctx, &users);
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let users = match db.auth_store().list_users().await {
        Ok(u) => u,
        Err(e) => exit_err(&e, ctx.json),
    };
    let rendered: Vec<serde_json::Value> = users.iter().map(user_row_json).collect();
    print_user_list(ctx, &rendered);
}

/// Resolve a user's ID from their name via the daemon's `GET /v1/users`
/// (daemon-routed path only — the direct-DB path resolves by name locally
/// via `AuthStore::get_user_by_name`).
async fn resolve_user_id_via_daemon(ctx: &CliContext, base_url: &str, name: &str) -> String {
    let url = format!("{base_url}/v1/users");
    let v = match daemon_request_async(ctx, reqwest::Method::GET, &url, None).await {
        Ok(v) => v,
        Err(e) => exit_err(&e, ctx.json),
    };
    v.as_array()
        .and_then(|users| users.iter().find(|u| u["name"] == name))
        .and_then(|u| u["id"].as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            exit_err(
                &Error::InvalidRequest {
                    message: format!("no user named '{name}'"),
                },
                ctx.json,
            )
        })
}

/// `localdb user remove <name>`
pub fn run_user_remove(ctx: &CliContext, name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_user_remove_async(ctx, name));
}

pub(crate) async fn run_user_remove_async(ctx: &CliContext, name: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let id = resolve_user_id_via_daemon(ctx, &base_url, name).await;
        let url = format!("{base_url}/v1/users/{id}");
        match daemon_request_async(ctx, reqwest::Method::DELETE, &url, None).await {
            Ok(_) => {
                if ctx.json {
                    print_json(&json!({ "status": "ok", "name": name }));
                } else {
                    println!("Removed user: {name} (via daemon)");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let user = match db.auth_store().get_user_by_name(name).await {
        Ok(Some(u)) => u,
        Ok(None) => exit_err(
            &Error::InvalidRequest {
                message: format!("no user named '{name}'"),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    };
    match db.auth_service().delete_user(&user.id).await {
        Ok(true) => {}
        Ok(false) => exit_err(
            &Error::InvalidRequest {
                message: format!("no user named '{name}'"),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "name": name }));
    } else {
        println!("Removed user: {name}");
    }
}

/// `localdb user set-role <name> <admin|member>`
pub fn run_user_set_role(ctx: &CliContext, name: &str, role: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_user_set_role_async(ctx, name, role));
}

pub(crate) async fn run_user_set_role_async(ctx: &CliContext, name: &str, role: &str) {
    let role = parse_role_arg(ctx, role);
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let id = resolve_user_id_via_daemon(ctx, &base_url, name).await;
        let url = format!("{base_url}/v1/users/{id}");
        let body = json!({ "role": role_to_str(role) });
        match daemon_request_async(ctx, reqwest::Method::PATCH, &url, Some(body)).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!("Set role of '{name}' to {} (via daemon)", role_to_str(role));
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let user = match db.auth_store().get_user_by_name(name).await {
        Ok(Some(u)) => u,
        Ok(None) => exit_err(
            &Error::InvalidRequest {
                message: format!("no user named '{name}'"),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    };
    if let Err(e) = db.auth_service().set_user_role(&user.id, role).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "name": name, "role": role_to_str(role) }));
    } else {
        println!("Set role of '{name}' to {}", role_to_str(role));
    }
}

fn key_view_json(t: &localdb_core::auth::AuthTokenRow) -> serde_json::Value {
    json!({
        "id": t.id,
        "created_at": t.created_at,
        "last_used_at": t.last_used_at,
        "expires_at": t.expires_at,
        "revoked_at": t.revoked_at,
    })
}

fn print_key_list(ctx: &CliContext, keys: &[serde_json::Value]) {
    if ctx.json {
        print_json(&json!({ "keys": keys }));
    } else if keys.is_empty() {
        println!("No keys.");
    } else {
        for k in keys {
            let revoked = if k["revoked_at"].is_string() {
                " (revoked)"
            } else {
                ""
            };
            println!("{}{}", k["id"].as_str().unwrap_or("?"), revoked);
        }
    }
}

/// `localdb key list [--user <name>]`
pub fn run_key_list(ctx: &CliContext, user_name: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_key_list_async(ctx, user_name));
}

pub(crate) async fn run_key_list_async(ctx: &CliContext, user_name: Option<&str>) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        // Resolve which user(s) to list keys for: the named one, or every
        // user (admin introspection — `key list` with no filter).
        let users_url = format!("{base_url}/v1/users");
        let all_users =
            match daemon_request_async(ctx, reqwest::Method::GET, &users_url, None).await {
                Ok(v) => v.as_array().cloned().unwrap_or_default(),
                Err(e) => exit_err(&e, ctx.json),
            };
        let targets: Vec<(String, String)> = all_users
            .iter()
            .filter(|u| user_name.is_none_or(|n| u["name"] == n))
            .filter_map(|u| {
                Some((
                    u["id"].as_str()?.to_string(),
                    u["name"].as_str()?.to_string(),
                ))
            })
            .collect();
        if let Some(n) = user_name {
            if targets.is_empty() {
                exit_err(
                    &Error::InvalidRequest {
                        message: format!("no user named '{n}'"),
                    },
                    ctx.json,
                );
            }
        }

        let mut all_keys = Vec::new();
        for (id, name) in &targets {
            let keys_url = format!("{base_url}/v1/users/{id}/keys");
            match daemon_request_async(ctx, reqwest::Method::GET, &keys_url, None).await {
                Ok(v) => {
                    for mut k in v.as_array().cloned().unwrap_or_default() {
                        if let Some(obj) = k.as_object_mut() {
                            obj.insert("user".to_string(), json!(name));
                        }
                        all_keys.push(k);
                    }
                }
                Err(e) => exit_err(&e, ctx.json),
            }
        }
        print_key_list(ctx, &all_keys);
        return;
    }

    let targets: Vec<localdb_core::auth::UserRow> = match user_name {
        Some(n) => match db.auth_store().get_user_by_name(n).await {
            Ok(Some(u)) => vec![u],
            Ok(None) => exit_err(
                &Error::InvalidRequest {
                    message: format!("no user named '{n}'"),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
        },
        None => match db.auth_store().list_users().await {
            Ok(u) => u,
            Err(e) => exit_err(&e, ctx.json),
        },
    };

    let mut all_keys = Vec::new();
    for user in &targets {
        let tokens = match db.auth_store().list_tokens_for_user(&user.id).await {
            Ok(t) => t,
            Err(e) => exit_err(&e, ctx.json),
        };
        for t in tokens
            .iter()
            .filter(|t| t.kind == localdb_core::auth::TokenKind::ApiKey)
        {
            let mut v = key_view_json(t);
            if let Some(obj) = v.as_object_mut() {
                obj.insert("user".to_string(), json!(user.name));
            }
            all_keys.push(v);
        }
    }
    print_key_list(ctx, &all_keys);
}

/// `localdb key revoke <id>`
pub fn run_key_revoke(ctx: &CliContext, key_id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_key_revoke_async(ctx, key_id));
}

pub(crate) async fn run_key_revoke_async(ctx: &CliContext, key_id: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/keys/{key_id}");
        match daemon_request_async(ctx, reqwest::Method::DELETE, &url, None).await {
            Ok(_) => {
                if ctx.json {
                    print_json(&json!({ "status": "ok", "id": key_id }));
                } else {
                    println!("Revoked key: {key_id} (via daemon)");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    match db.auth_store().find_token(key_id).await {
        Ok(Some(_)) => {}
        Ok(None) => exit_err(
            &Error::InvalidRequest {
                message: format!("no key with id '{key_id}'"),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }
    if let Err(e) = db.auth_store().revoke_token(key_id).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "id": key_id }));
    } else {
        println!("Revoked key: {key_id}");
    }
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
