//! Invite management: `localdb invite create|list|revoke|requests|approve|deny`
//! (specs/05-surfaces.md §2). Admin-only, following the same
//! daemon-routed-first, direct-DB-fallback pattern as `cli/src/cmds/auth.rs`
//! (`user`/`key`) and `cli/src/cmds/store.rs` (`store grant/revoke`): when a
//! daemon is reachable, requests go over HTTP with the caller's bearer
//! (`Principal::require_admin` on the server side maps a non-admin bearer to
//! `forbidden`/exit 6); otherwise this falls back to a direct, trusted
//! database read/write.
//!
//! No `--direct-db` escape hatch here (unlike `user add`/`key create`):
//! invites aren't a lockout-recovery primitive, so there is no reason to
//! force past a running daemon.

use localdb_core::{
    auth::{AuthStore as _, InviteMode},
    Error,
};
use serde_json::json;

use crate::{
    app_db::load_app_db,
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json},
};

fn invite_mode_to_str(mode: InviteMode) -> &'static str {
    match mode {
        InviteMode::Open => "open",
        InviteMode::Closed => "closed",
    }
}

fn parse_invite_mode_arg(ctx: &CliContext, s: &str) -> InviteMode {
    match s {
        "open" => InviteMode::Open,
        "closed" => InviteMode::Closed,
        other => exit_err(
            &Error::InvalidRequest {
                message: format!("unknown invite mode '{other}'; expected 'open' or 'closed'"),
            },
            ctx.json,
        ),
    }
}

/// Parse a human-readable duration (`7d`, `24h`, `30m`, `3600s`, or a bare
/// number of seconds) into seconds. Extends
/// `localdb_core::config::validate_refresh_interval`'s d/h/m/s convention
/// with a `d` (days) suffix, which invite expiries need (`--expires 7d`) but
/// source refresh intervals never have — kept local to this command rather
/// than added to the shared helper so as not to change refresh-interval
/// parsing's accepted syntax.
pub(crate) fn parse_expiry_duration(s: &str) -> Result<u64, Error> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidRequest {
            message: "expiry duration must not be empty".to_string(),
        });
    }
    let secs = if let Some(d) = trimmed.strip_suffix('d') {
        d.parse::<u64>().ok().and_then(|n| n.checked_mul(86400))
    } else if let Some(h) = trimmed.strip_suffix('h') {
        h.parse::<u64>().ok().and_then(|n| n.checked_mul(3600))
    } else if let Some(m) = trimmed.strip_suffix('m') {
        m.parse::<u64>().ok().and_then(|n| n.checked_mul(60))
    } else if let Some(sec) = trimmed.strip_suffix('s') {
        sec.parse::<u64>().ok()
    } else {
        trimmed.parse::<u64>().ok()
    };
    match secs {
        None => Err(Error::InvalidRequest {
            message: format!(
                "invalid expiry duration '{trimmed}': expected a duration like '7d', '24h', \
                 '30m', or '3600s'"
            ),
        }),
        Some(0) => Err(Error::InvalidRequest {
            message: format!(
                "invalid expiry duration '{trimmed}': duration must be greater than zero"
            ),
        }),
        Some(n) => Ok(n),
    }
}

fn invite_row_json(row: &localdb_core::auth::InviteRow) -> serde_json::Value {
    json!({
        "id": row.id,
        "mode": invite_mode_to_str(row.mode),
        "store_grants": row.store_grants,
        "max_uses": row.max_uses,
        "uses": row.uses,
        "expires_at": row.expires_at,
        "revoked_at": row.revoked_at,
        "created_by": row.created_by,
        "created_at": row.created_at,
    })
}

fn access_request_state_str(s: localdb_core::auth::AccessRequestState) -> &'static str {
    use localdb_core::auth::AccessRequestState::*;
    match s {
        Pending => "pending",
        Approved => "approved",
        Denied => "denied",
    }
}

fn access_request_row_json(row: &localdb_core::auth::AccessRequestRow) -> serde_json::Value {
    json!({
        "id": row.id,
        "invite_id": row.invite_id,
        "requested_name": row.requested_name,
        "state": access_request_state_str(row.state),
        "resulting_user_id": row.resulting_user_id,
        "created_at": row.created_at,
        "decided_at": row.decided_at,
    })
}

/// `localdb invite create --mode open|closed [--store <name>]... [--expires
/// <duration>] [--max-uses N]`
pub fn run_invite_create(
    ctx: &CliContext,
    mode: &str,
    stores: &[String],
    expires: Option<&str>,
    max_uses: u32,
) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_invite_create_async(
        ctx, mode, stores, expires, max_uses,
    ));
}

pub(crate) async fn run_invite_create_async(
    ctx: &CliContext,
    mode: &str,
    stores: &[String],
    expires: Option<&str>,
    max_uses: u32,
) {
    let mode_enum = parse_invite_mode_arg(ctx, mode);
    let expires_at = match expires {
        Some(s) => match parse_expiry_duration(s) {
            Ok(secs) => Some(localdb_core::auth::rfc3339_from_now(secs as i64)),
            Err(e) => exit_err(&e, ctx.json),
        },
        None => None,
    };

    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/invites");
        let body = json!({
            "mode": mode,
            "stores": stores,
            "max_uses": max_uses,
            "expires_at": expires_at,
        });
        match daemon_request_async(ctx, reqwest::Method::POST, &url, Some(body)).await {
            Ok(v) => {
                print_created_invite(ctx, &v, true);
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let mut store_grants = Vec::with_capacity(stores.len());
    for name in stores {
        let visibility = match db.backend().get_store_by_name(name).await {
            Ok(Some(row)) => row.visibility,
            Ok(None) => exit_err(
                &Error::StoreNotFound {
                    id: name.to_string(),
                },
                ctx.json,
            ),
            Err(e) => exit_err(&e, ctx.json),
        };
        store_grants.push((name.clone(), visibility));
    }

    let issued = match db
        .auth_service()
        .create_invite(mode_enum, &store_grants, max_uses, expires_at, "local")
        .await
    {
        Ok(issued) => issued,
        Err(e) => exit_err(&e, ctx.json),
    };

    let mut rendered = invite_row_json(&issued.row);
    if let Some(obj) = rendered.as_object_mut() {
        obj.insert("token".to_string(), json!(issued.secret));
    }
    print_created_invite(ctx, &rendered, false);
}

fn print_created_invite(ctx: &CliContext, v: &serde_json::Value, via_daemon: bool) {
    let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
    if ctx.json {
        print_json(v);
        return;
    }
    let suffix = if via_daemon { " (via daemon)" } else { "" };
    println!(
        "Created invite {}{suffix}",
        v.get("id").and_then(|i| i.as_str()).unwrap_or("?")
    );
    println!("Token: {token}");
    println!("Store this now — it is shown only once and cannot be recovered.");
    match v.get("consent_url").and_then(|c| c.as_str()) {
        Some(url) => println!("Consent URL: {url}"),
        None => println!(
            "Consent URL: start a daemon and visit <daemon base url>/authorize?invite={token}"
        ),
    }
}

/// `localdb invite list`
pub fn run_invite_list(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_invite_list_async(ctx));
}

pub(crate) async fn run_invite_list_async(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/invites");
        match daemon_request_async(ctx, reqwest::Method::GET, &url, None).await {
            Ok(v) => {
                print_invite_list(ctx, v.as_array().cloned().unwrap_or_default());
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let invites = match db.auth_store().list_invites().await {
        Ok(i) => i,
        Err(e) => exit_err(&e, ctx.json),
    };
    print_invite_list(ctx, invites.iter().map(invite_row_json).collect());
}

fn print_invite_list(ctx: &CliContext, invites: Vec<serde_json::Value>) {
    if ctx.json {
        print_json(&json!({ "invites": invites }));
    } else if invites.is_empty() {
        println!("No invites.");
    } else {
        for i in &invites {
            let revoked = if i.get("revoked_at").is_some_and(|v| v.is_string()) {
                " (revoked)"
            } else {
                ""
            };
            println!(
                "{} [{}] uses {}/{}{revoked}",
                i["id"].as_str().unwrap_or("?"),
                i["mode"].as_str().unwrap_or("?"),
                i["uses"].as_u64().unwrap_or(0),
                i["max_uses"].as_u64().unwrap_or(0),
            );
        }
    }
}

/// `localdb invite revoke <id>`
pub fn run_invite_revoke(ctx: &CliContext, id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_invite_revoke_async(ctx, id));
}

pub(crate) async fn run_invite_revoke_async(ctx: &CliContext, id: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/invites/{id}");
        match daemon_request_async(ctx, reqwest::Method::DELETE, &url, None).await {
            Ok(_) => {
                if ctx.json {
                    print_json(&json!({ "status": "ok", "id": id }));
                } else {
                    println!("Revoked invite: {id} (via daemon)");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    match db.auth_store().revoke_invite(id).await {
        Ok(true) => {}
        Ok(false) => exit_err(
            &Error::InvalidRequest {
                message: format!("invite '{id}' not found or already revoked"),
            },
            ctx.json,
        ),
        Err(e) => exit_err(&e, ctx.json),
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "id": id }));
    } else {
        println!("Revoked invite: {id}");
    }
}

/// `localdb invite requests`
pub fn run_invite_requests(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_invite_requests_async(ctx));
}

pub(crate) async fn run_invite_requests_async(ctx: &CliContext) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/invites/requests");
        match daemon_request_async(ctx, reqwest::Method::GET, &url, None).await {
            Ok(v) => {
                print_requests_list(ctx, v.as_array().cloned().unwrap_or_default());
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let requests = match db.auth_store().list_access_requests().await {
        Ok(r) => r,
        Err(e) => exit_err(&e, ctx.json),
    };
    print_requests_list(ctx, requests.iter().map(access_request_row_json).collect());
}

fn print_requests_list(ctx: &CliContext, requests: Vec<serde_json::Value>) {
    if ctx.json {
        print_json(&json!({ "requests": requests }));
    } else if requests.is_empty() {
        println!("No access requests.");
    } else {
        for r in &requests {
            println!(
                "{} '{}' [{}]",
                r["id"].as_str().unwrap_or("?"),
                r["requested_name"].as_str().unwrap_or("?"),
                r["state"].as_str().unwrap_or("?"),
            );
        }
    }
}

/// `localdb invite approve <request-id>`
pub fn run_invite_approve(ctx: &CliContext, request_id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_invite_approve_async(ctx, request_id));
}

pub(crate) async fn run_invite_approve_async(ctx: &CliContext, request_id: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/invites/requests/{request_id}/approve");
        match daemon_request_async(ctx, reqwest::Method::POST, &url, None).await {
            Ok(v) => {
                if ctx.json {
                    print_json(&v);
                } else {
                    println!(
                        "Approved request {request_id}: user '{}' created (via daemon)",
                        v.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
                    );
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    let user = match db.auth_service().approve_request(request_id).await {
        Ok(u) => u,
        Err(e) => exit_err(&e, ctx.json),
    };

    if ctx.json {
        print_json(&json!({ "status": "ok", "id": request_id, "name": user.name }));
    } else {
        println!(
            "Approved request {request_id}: user '{}' created",
            user.name
        );
    }
}

/// `localdb invite deny <request-id>`
pub fn run_invite_deny(ctx: &CliContext, request_id: &str) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_invite_deny_async(ctx, request_id));
}

pub(crate) async fn run_invite_deny_async(ctx: &CliContext, request_id: &str) {
    let (config_loader, db) = load_app_db(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    if let DaemonState::Running { base_url } = probe_daemon(data_dir, ctx.daemon_url.as_deref()) {
        let url = format!("{base_url}/v1/invites/requests/{request_id}/deny");
        match daemon_request_async(ctx, reqwest::Method::POST, &url, None).await {
            Ok(_) => {
                if ctx.json {
                    print_json(&json!({ "status": "ok", "id": request_id }));
                } else {
                    println!("Denied request {request_id} (via daemon)");
                }
                return;
            }
            Err(e) => exit_err(&e, ctx.json),
        }
    }

    if let Err(e) = db.auth_service().deny_request(request_id).await {
        exit_err(&e, ctx.json);
    }

    if ctx.json {
        print_json(&json!({ "status": "ok", "id": request_id }));
    } else {
        println!("Denied request {request_id}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_db::AppDb;
    use localdb_core::auth::Role;
    use localdb_core::config::loader::ResolvedPaths;
    use localdb_core::config::schema::{EmbeddingPolicy, IndexingPolicyConfig};
    use localdb_core::types::StoreVisibility;
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

    #[test]
    fn parse_expiry_duration_supports_days() {
        assert_eq!(parse_expiry_duration("7d").unwrap(), 7 * 86400);
    }

    #[test]
    fn parse_expiry_duration_supports_hours_minutes_seconds() {
        assert_eq!(parse_expiry_duration("24h").unwrap(), 86400);
        assert_eq!(parse_expiry_duration("30m").unwrap(), 1800);
        assert_eq!(parse_expiry_duration("3600s").unwrap(), 3600);
        assert_eq!(parse_expiry_duration("60").unwrap(), 60);
    }

    #[test]
    fn parse_expiry_duration_rejects_zero_and_garbage() {
        assert!(parse_expiry_duration("0d").is_err());
        assert!(parse_expiry_duration("nonsense").is_err());
        assert!(parse_expiry_duration("").is_err());
    }

    #[tokio::test]
    async fn create_invite_direct_db_then_list_and_revoke() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        db.auth_service()
            .create_user("admin", Role::Admin)
            .await
            .unwrap();

        let issued = db
            .auth_service()
            .create_invite(InviteMode::Open, &[], 1, None, "admin")
            .await
            .unwrap();
        assert!(issued.secret.starts_with("ldb_"));

        let all = db.auth_store().list_invites().await.unwrap();
        assert_eq!(all.len(), 1);

        assert!(db.auth_store().revoke_invite(&issued.row.id).await.unwrap());
    }

    #[tokio::test]
    async fn create_invite_with_store_grant_resolves_visibility_direct_db() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let store = localdb_core::StoreRow {
            id: "store-docs".to_string(),
            name: "docs".to_string(),
            visibility: StoreVisibility::Shared,
            backend: "libsql".to_string(),
            indexing_policy: "{}".to_string(),
            policy_version: "v1".to_string(),
            acl: "{}".to_string(),
            created_at: localdb_core::ingestion::now_rfc3339(),
        };
        db.backend().upsert_store(&store).await.unwrap();

        let issued = db
            .auth_service()
            .create_invite(
                InviteMode::Open,
                &[("docs".to_string(), StoreVisibility::Shared)],
                1,
                None,
                "admin",
            )
            .await
            .unwrap();
        assert_eq!(issued.row.store_grants, vec!["docs".to_string()]);
    }

    #[tokio::test]
    async fn approve_and_deny_direct_db_round_trip() {
        let dir = TempDir::new().unwrap();
        let db = tmp_app_db(&dir).await;
        let issued = db
            .auth_service()
            .create_invite(InviteMode::Closed, &[], 2, None, "admin")
            .await
            .unwrap();

        let outcome = db
            .auth_service()
            .redeem_invite(&issued.secret, "alice")
            .await
            .unwrap();
        let localdb_core::auth::RedeemOutcome::Closed { request_id, .. } = outcome else {
            panic!("expected Closed outcome");
        };
        let user = db
            .auth_service()
            .approve_request(&request_id)
            .await
            .unwrap();
        assert_eq!(user.name, "alice");

        let outcome2 = db
            .auth_service()
            .redeem_invite(&issued.secret, "bob")
            .await
            .unwrap();
        let localdb_core::auth::RedeemOutcome::Closed {
            request_id: request_id2,
            ..
        } = outcome2
        else {
            panic!("expected Closed outcome");
        };
        db.auth_service().deny_request(&request_id2).await.unwrap();

        let requests = db.auth_store().list_access_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
    }
}
