use serde_json::json;

use localdb_core::config::loader::ConfigLoader;

use crate::{
    app_db::load_app_db_lenient,
    daemon_client::{probe_daemon, CliContext, DaemonState},
    normalize::{exit_err, print_json, visibility_to_string},
};

/// `localdb status`
pub fn run_status(ctx: &CliContext) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_status_async(ctx));
}

pub(crate) async fn run_status_async(ctx: &CliContext) {
    // F1-cli: use lenient loader so status works even with malformed config.
    let (config_loader, db) = load_app_db_lenient(ctx).await;
    let data_dir = &config_loader.paths.data_dir;

    let daemon_state = probe_daemon(data_dir, ctx.daemon_url.as_deref());
    let daemon_status = match &daemon_state {
        DaemonState::Running { base_url } => format!("running ({})", base_url),
        DaemonState::NotRunning => "not running (embedded mode)".to_string(),
    };

    // Issue #98: show the caller's identity + cached token expiry when
    // authenticated against a running daemon. Best-effort — any failure
    // (no cached credential, daemon unreachable, expired token) degrades
    // silently to "no identity shown" rather than failing `status` itself.
    let identity = match &daemon_state {
        DaemonState::Running { base_url } => fetch_identity(ctx, &config_loader, base_url).await,
        DaemonState::NotRunning => None,
    };

    let runtime_stores = match db.backend().list_stores().await {
        Ok(s) => s,
        Err(e) => exit_err(&e, ctx.json),
    };

    let all_stores: Vec<serde_json::Value> = runtime_stores
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "visibility": visibility_to_string(&s.visibility),
                "backend": s.backend,
            })
        })
        .collect();

    if ctx.json {
        let mut value = json!({
            "daemon": daemon_status,
            "stores": all_stores,
        });
        if let Some(identity) = &identity {
            value["identity"] = identity.clone();
        }
        print_json(&value);
    } else {
        println!("daemon: {}", daemon_status);
        if let Some(identity) = &identity {
            println!(
                "identity: {} ({})",
                identity["name"].as_str().unwrap_or("?"),
                identity["role"].as_str().unwrap_or("?"),
            );
            if let Some(expiry) = identity.get("access_expires_at").and_then(|v| v.as_str()) {
                println!("token expires: {}", expiry);
            }
        }
        println!("stores ({}):", all_stores.len());
        if all_stores.is_empty() {
            println!("  (none)");
        }
        for s in &all_stores {
            println!(
                "  {} [{}]",
                s["name"].as_str().unwrap_or("?"),
                s["backend"].as_str().unwrap_or("?"),
            );
        }
    }
}

/// Fetch the caller's identity from `{base_url}/v1/auth/me`, using whatever
/// cached bearer credential resolves for that base URL (`LOCALDB_API_KEY`,
/// then `credentials.json` — same priority as every other daemon-attached
/// command). Attaches the cached `access_expires_at`, if any, to the
/// response so `status` can show token expiry (issue #98) even though
/// `/v1/auth/me` itself doesn't know about locally-cached expiry.
/// Returns `None` on any failure — no cached credential, an unreachable
/// daemon, or a rejected token — so `status` always degrades gracefully
/// rather than failing.
async fn fetch_identity(
    ctx: &CliContext,
    config_loader: &ConfigLoader,
    base_url: &str,
) -> Option<serde_json::Value> {
    let credentials_file = crate::credentials::credentials_path(&config_loader.paths.config_file);
    let entry = crate::credentials::lookup_entry(&credentials_file, base_url);
    let bearer = crate::credentials::resolve_bearer(
        ctx.api_key.as_deref(),
        Some(&config_loader.paths.config_file),
        base_url,
    )?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client
        .get(format!("{base_url}/v1/auth/me"))
        .bearer_auth(&bearer)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mut me: serde_json::Value = resp.json().await.ok()?;
    if let Some(entry) = entry {
        if let Some(expiry) = entry.access_expires_at {
            me["access_expires_at"] = serde_json::Value::String(expiry);
        }
    }
    Some(me)
}
