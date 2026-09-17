//! `localdb login` / `logout` (specs/05-surfaces.md §2, §3.1, T4).
//!
//! `login` drives the OAuth2 authorization-code + PKCE flow
//! (`localdb_core::auth::generate_pkce_pair`/`OOB_REDIRECT_URI`) against a
//! running daemon's `/authorize` + `/token` routes and caches the resulting
//! access/refresh token pair in `credentials.json`
//! (`crate::credentials::write_entry`). `logout` revokes the cached tokens
//! (`/revoke`, best-effort) and clears the cached entry.
//!
//! No domain logic lives here (specs/01-architecture.md §1) — this module
//! is thin HTTP-client orchestration: PKCE/state generation is
//! `core::auth::generate_pkce_pair`, the auth-code state machine and
//! redirect-uri policy are `server`/`core::auth`, this just drives the HTTP
//! round trips a browser would.

use localdb_core::{
    config::loader::{load_config, LoadOptions},
    Error,
};
use serde_json::json;

use crate::{
    credentials::CredentialEntry,
    daemon_client::{probe_daemon, resolved_config_file, CliContext, DaemonState},
    normalize::{exit_err, print_json},
};

const CLIENT_ID: &str = "localdb-cli";

/// `localdb login [--url <base>] [--setup-code <code>] [--no-browser]
/// [--invite <token>] [--name <name>]`
///
/// T6: `--invite` takes an entirely different, browser-free path
/// (`perform_invite_login`) — see its doc comment for why direct redemption
/// is primary over forwarding to the `/authorize?invite=` consent page.
#[allow(clippy::too_many_arguments)]
pub fn run_login(
    ctx: &CliContext,
    url: Option<&str>,
    setup_code: Option<&str>,
    no_browser: bool,
    invite: Option<&str>,
    name: Option<&str>,
) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_login_async(
        ctx, url, setup_code, no_browser, invite, name,
    ));
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_login_async(
    ctx: &CliContext,
    url: Option<&str>,
    setup_code: Option<&str>,
    no_browser: bool,
    invite: Option<&str>,
    name: Option<&str>,
) {
    let base_url = resolve_login_base_url(ctx, url).await;

    if let Some(token) = invite {
        match perform_invite_login(
            ctx,
            &base_url,
            token,
            name,
            std::time::Duration::from_secs(1),
        )
        .await
        {
            Ok(joined_name) => {
                if ctx.json {
                    print_json(
                        &json!({ "status": "ok", "base_url": base_url, "name": joined_name }),
                    );
                } else {
                    println!("Logged in to {base_url} as '{joined_name}'.");
                }
            }
            Err(e) => exit_err(&e, ctx.json),
        }
        return;
    }

    let opener = |u: &str| webbrowser::open(u).is_ok();

    match perform_login(
        ctx,
        &base_url,
        setup_code,
        no_browser,
        opener,
        std::time::Duration::from_secs(300),
    )
    .await
    {
        Ok(_summary) => {
            if ctx.json {
                print_json(&json!({ "status": "ok", "base_url": base_url }));
            } else {
                println!("Logged in to {base_url}.");
            }
        }
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// The requested user name for an invite redemption: `--name` if given,
/// else the OS login name (`$USER`/`%USERNAME%`) as a friendly default so
/// the common case doesn't require typing a name twice (once to log into
/// the OS, once to join localdb). Empty/missing either way is a usage
/// error (exit 2) — there is no further fallback that wouldn't be
/// surprising.
fn resolve_requested_name(name: Option<&str>) -> Result<String, Error> {
    if let Some(n) = name {
        let trimmed = n.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Some(n) = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("USERNAME").ok())
        .filter(|s| !s.trim().is_empty())
    {
        return Ok(n.trim().to_string());
    }
    Err(Error::InvalidRequest {
        message: "no name given and none could be inferred from the environment; pass --name"
            .to_string(),
    })
}

/// `localdb login --invite <token>` (T6): redeems the invite directly
/// against `POST /v1/invites/redeem` — no browser round trip needed. This
/// is the *primary* closed-mode path (the consent page's invite branch,
/// `server::auth::oauth::handle_invite_authorize`, exists too, but only for
/// the browser flow; it can't drive a poll loop the way this can). `open`
/// mode: one request, done. `closed` mode: polls
/// `GET /v1/invites/requests/{id}` every `poll_interval` until an admin
/// decides. `poll_interval` is injected (production: 1s) so tests can drive
/// the loop with a much shorter interval instead of a multi-second sleep.
async fn perform_invite_login(
    ctx: &CliContext,
    base_url: &str,
    invite_token: &str,
    name: Option<&str>,
    poll_interval: std::time::Duration,
) -> Result<String, Error> {
    let requested_name = resolve_requested_name(name)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Internal {
            message: format!("cannot build HTTP client: {e}"),
            correlation_id: "login_invite_client".to_string(),
        })?;

    let resp = client
        .post(format!("{base_url}/v1/invites/redeem"))
        .json(&json!({ "token": invite_token, "name": requested_name }))
        .send()
        .await
        .map_err(|_| Error::DaemonUnreachable)?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

    if status == reqwest::StatusCode::CREATED {
        // Open mode: the credential is ready immediately.
        let api_key = body
            .get("api_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Internal {
                message: "redeem response is missing api_key".to_string(),
                correlation_id: "login_invite_open_response".to_string(),
            })?;
        persist_api_key(ctx, base_url, api_key).await?;
        return Ok(requested_name);
    }

    if status == reqwest::StatusCode::ACCEPTED {
        let request_id = body
            .get("request_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Internal {
                message: "redeem response is missing request_id".to_string(),
                correlation_id: "login_invite_closed_response".to_string(),
            })?
            .to_string();
        let request_secret = body
            .get("request_secret")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Internal {
                message: "redeem response is missing request_secret".to_string(),
                correlation_id: "login_invite_closed_response".to_string(),
            })?
            .to_string();

        if !ctx.json {
            println!(
                "Waiting for admin approval (request id: {request_id})... \
                 ask an admin to run `localdb invite approve {request_id}`."
            );
        }

        let api_key = poll_until_decided(
            &client,
            base_url,
            &request_id,
            &request_secret,
            poll_interval,
        )
        .await?;
        persist_api_key(ctx, base_url, &api_key).await?;
        return Ok(requested_name);
    }

    Err(crate::daemon_client::daemon_response_error(status, &body))
}

/// Poll `GET /v1/invites/requests/{id}?secret=...` every `poll_interval`
/// until the request is decided. Returns the credential on approval;
/// `denied` is a terminal `Unauthorized` with a clear message, per
/// specs/05-surfaces.md §2.
async fn poll_until_decided(
    client: &reqwest::Client,
    base_url: &str,
    request_id: &str,
    request_secret: &str,
    poll_interval: std::time::Duration,
) -> Result<String, Error> {
    loop {
        let resp = client
            .get(format!(
                "{base_url}/v1/invites/requests/{request_id}?secret={request_secret}"
            ))
            .send()
            .await
            .map_err(|_| Error::DaemonUnreachable)?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            return Err(crate::daemon_client::daemon_response_error(status, &body));
        }
        match body.get("state").and_then(|v| v.as_str()) {
            Some("approved") => {
                return body
                    .get("api_key")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| Error::Internal {
                        message: "poll response is missing api_key on approval".to_string(),
                        correlation_id: "login_invite_poll_response".to_string(),
                    });
            }
            Some("denied") => {
                return Err(Error::Unauthorized {
                    message: "the access request was denied by an admin".to_string(),
                });
            }
            Some("collected") => {
                // Should not normally happen (this loop is the one and only
                // collector), but fail closed rather than loop forever.
                return Err(Error::Internal {
                    message: "the request's credential was already collected".to_string(),
                    correlation_id: "login_invite_poll_already_collected".to_string(),
                });
            }
            Some("pending") => tokio::time::sleep(poll_interval).await,
            _ => {
                return Err(Error::Internal {
                    message: "invalid approval response".into(),
                    correlation_id: "login_invite_poll_response".into(),
                })
            }
        }
    }
}

/// Persist a redeemed invite's API key into `credentials.json` under the
/// legacy `secret` shape (D1: API keys have no default expiry, unlike the
/// OAuth access/refresh pair `perform_login` writes).
async fn persist_api_key(ctx: &CliContext, base_url: &str, api_key: &str) -> Result<(), Error> {
    let config_file = resolved_config_file(ctx).ok_or_else(|| Error::InvalidConfig {
        message: "cannot resolve the config file path to write credentials.json".to_string(),
    })?;
    let credentials_file = crate::credentials::credentials_path(&config_file);
    let entry = CredentialEntry {
        secret: Some(api_key.to_string()),
        access_token: None,
        refresh_token: None,
        access_expires_at: None,
    };
    write_credential(&credentials_file, base_url, entry)
        .await
        .map_err(|e| Error::Internal {
            message: format!("failed to write credentials.json: {e}"),
            correlation_id: "login_invite_persist_write".to_string(),
        })
}

async fn write_credential(
    path: &std::path::Path,
    base_url: &str,
    entry: CredentialEntry,
) -> std::io::Result<()> {
    crate::credentials::CredentialLock::acquire(path)
        .await?
        .write(base_url, entry)
}

/// `localdb logout [--url <base>]`
pub fn run_logout(ctx: &CliContext, url: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_logout_async(ctx, url));
}

pub(crate) async fn run_logout_async(ctx: &CliContext, url: Option<&str>) {
    let base_url = resolve_login_base_url(ctx, url).await;
    let Some(config_file) = resolved_config_file(ctx) else {
        exit_err(
            &Error::InvalidConfig {
                message: "cannot resolve the config file path to locate credentials.json"
                    .to_string(),
            },
            ctx.json,
        );
    };
    let credentials_file = crate::credentials::credentials_path(&config_file);

    let removed = match revoke_and_remove(&credentials_file, &base_url).await {
        Ok(removed) => removed,
        Err(error) => exit_err(&error, ctx.json),
    };
    if ctx.json {
        print_json(&json!({ "status": "ok", "base_url": base_url, "removed": removed }));
    } else if removed {
        println!("Logged out of {base_url}.");
    } else {
        println!("No cached credentials found for {base_url}.");
    }
}

async fn revoke_and_remove(path: &std::path::Path, base_url: &str) -> Result<bool, Error> {
    let storage_error = |e: std::io::Error| Error::Internal {
        message: format!("credential cache: {e}"),
        correlation_id: "logout_cache".into(),
    };
    let lock = crate::credentials::CredentialLock::acquire(path)
        .await
        .map_err(storage_error)?;
    if let Some(entry) = lock.lookup(base_url) {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Internal {
                message: e.to_string(),
                correlation_id: "logout_client".into(),
            })?;
        for secret in [entry.access_token, entry.refresh_token, entry.secret]
            .into_iter()
            .flatten()
        {
            let response = client
                .post(format!("{base_url}/revoke"))
                .form(&[("token", secret)])
                .send()
                .await
                .map_err(|_| Error::DaemonUnreachable)?;
            if !response.status().is_success() {
                return Err(Error::Internal {
                    message: "logout could not revoke credentials; cache retained, retry logout"
                        .into(),
                    correlation_id: "logout_revoke".into(),
                });
            }
        }
    }
    lock.remove(base_url).map_err(storage_error)
}

/// Resolve the daemon base URL to log in/out against: `--url` wins; else
/// probe for a running daemon (login/logout only make sense against a
/// daemon — there is no auth to log into without one, specs/05-surfaces.md
/// §2).
async fn resolve_login_base_url(ctx: &CliContext, url: Option<&str>) -> String {
    if let Some(u) = url {
        return u.trim_end_matches('/').to_string();
    }
    let options = LoadOptions {
        config_path: ctx.config.clone(),
        ..Default::default()
    };
    let config_loader = match load_config(&options, ctx.config_env.as_deref()) {
        Ok(c) => c,
        Err(e) => exit_err(&e, ctx.json),
    };
    match probe_daemon(&config_loader.paths.data_dir, ctx.daemon_url.as_deref()) {
        DaemonState::Running { base_url } => base_url,
        DaemonState::NotRunning => exit_err(&Error::DaemonUnreachable, ctx.json),
    }
}

struct LoginSummary {
    #[allow(dead_code)]
    base_url: String,
}

/// Drive the authorization-code + PKCE flow end to end. `open_browser` is
/// injected so tests can substitute a fake "browser" (an HTTP client that
/// submits the consent form) instead of actually launching one; production
/// code passes `|u| webbrowser::open(u).is_ok()`. `callback_timeout` bounds
/// how long we wait for the loopback callback (production: 5 minutes for a
/// human to click through; tests use a much shorter bound so a
/// never-arriving callback — e.g. a rejected credential — fails fast
/// instead of stalling the suite).
async fn perform_login(
    ctx: &CliContext,
    base_url: &str,
    setup_code: Option<&str>,
    no_browser: bool,
    open_browser: impl Fn(&str) -> bool,
    callback_timeout: std::time::Duration,
) -> Result<LoginSummary, Error> {
    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let (csrf_state, _) = localdb_core::auth::generate_pkce_pair();

    let (redirect_uri, code, returned_state) = if no_browser {
        let redirect_uri = localdb_core::auth::OOB_REDIRECT_URI.to_string();
        let authorize_url =
            build_authorize_url(base_url, &redirect_uri, &csrf_state, &challenge, setup_code);
        println!("Open this URL in a browser to authorize `localdb login`:\n\n  {authorize_url}\n");
        print!("Paste the code shown after authorizing: ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::Internal {
                message: format!("failed to read the pasted code from stdin: {e}"),
                correlation_id: "login_stdin".to_string(),
            })?;
        // No real callback round trip happened in oob mode, so there is no
        // `state` to check for CSRF — trust our own freshly generated value.
        (redirect_uri, line.trim().to_string(), csrf_state.clone())
    } else {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| Error::Internal {
                message: format!("cannot bind a local callback listener: {e}"),
                correlation_id: "login_listener_bind".to_string(),
            })?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Internal {
                message: format!("cannot read the local callback listener's address: {e}"),
                correlation_id: "login_listener_addr".to_string(),
            })?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let authorize_url =
            build_authorize_url(base_url, &redirect_uri, &csrf_state, &challenge, setup_code);

        if !open_browser(&authorize_url) {
            println!(
                "Could not open a browser automatically. Open this URL to continue:\n\n  {authorize_url}\n"
            );
        }

        let (code, returned_state) = wait_for_callback(listener, callback_timeout).await?;
        (redirect_uri, code, returned_state)
    };

    if returned_state != csrf_state {
        return Err(Error::Unauthorized {
            message: "state parameter mismatch on the login callback; aborting (possible CSRF)"
                .to_string(),
        });
    }
    if code.is_empty() {
        return Err(Error::InvalidRequest {
            message: "no authorization code was received".to_string(),
        });
    }

    let (access_token, refresh_token, expires_in) =
        exchange_code_for_tokens(base_url, &code, &redirect_uri, &verifier).await?;

    let config_file = resolved_config_file(ctx).ok_or_else(|| Error::InvalidConfig {
        message: "cannot resolve the config file path to write credentials.json".to_string(),
    })?;
    let credentials_file = crate::credentials::credentials_path(&config_file);
    let entry = CredentialEntry {
        secret: None,
        access_token: Some(access_token),
        refresh_token,
        access_expires_at: Some(localdb_core::auth::rfc3339_from_now(expires_in)),
    };
    write_credential(&credentials_file, base_url, entry)
        .await
        .map_err(|e| Error::Internal {
            message: format!("failed to write credentials.json: {e}"),
            correlation_id: "login_persist_write".to_string(),
        })?;

    Ok(LoginSummary {
        base_url: base_url.to_string(),
    })
}

fn build_authorize_url(
    base_url: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    setup_code: Option<&str>,
) -> String {
    let mut pairs = vec![
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    if let Some(sc) = setup_code {
        pairs.push(("setup_code", sc));
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    format!("{base_url}/authorize?{query}")
}

/// Accept exactly one connection on `listener` (the ephemeral loopback
/// callback), parse `code`/`state` (or `error`) off its request line, reply
/// with a minimal "you can close this window" page, and return what was
/// found. A tiny hand-rolled accept loop, per design — no HTTP server crate
/// needed for a single one-shot request.
async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    timeout: std::time::Duration,
) -> Result<(String, String), Error> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (mut stream, _) = tokio::time::timeout(timeout, listener.accept())
        .await
        .map_err(|_| Error::Unauthorized {
            message: "timed out waiting for the browser to complete login".to_string(),
        })?
        .map_err(|e| Error::Internal {
            message: format!("callback listener accept failed: {e}"),
            correlation_id: "login_callback_accept".to_string(),
        })?;

    let (read_half, mut write_half) = stream.split();
    let mut reader = BufReader::new(read_half);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .map_err(|e| Error::Internal {
            message: format!("callback read failed: {e}"),
            correlation_id: "login_callback_read".to_string(),
        })?;

    // Drain the remaining request headers (we don't need them).
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| Error::Internal {
                message: format!("callback header read failed: {e}"),
                correlation_id: "login_callback_headers".to_string(),
            })?;
        if n == 0 || line == "\r\n" {
            break;
        }
    }

    let path_and_query = request_line.split_whitespace().nth(1).unwrap_or("");
    let query = path_and_query.split_once('?').map(|x| x.1).unwrap_or("");
    let params: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();

    let body = "<html><body>Login complete &mdash; you can close this window.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = write_half.write_all(response.as_bytes()).await;
    let _ = write_half.shutdown().await;

    if let Some(err) = params.get("error") {
        return Err(Error::Unauthorized {
            message: format!("authorization failed: {err}"),
        });
    }
    let code = params.get("code").cloned().unwrap_or_default();
    let state = params.get("state").cloned().unwrap_or_default();
    Ok((code, state))
}

async fn exchange_code_for_tokens(
    base_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<(String, Option<String>, i64), Error> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Internal {
            message: format!("cannot build HTTP client: {e}"),
            correlation_id: "login_token_client".to_string(),
        })?;
    let resp = client
        .post(format!("{base_url}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|_| Error::DaemonUnreachable)?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let msg = json
            .get("error_description")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("error").and_then(|v| v.as_str()))
            .unwrap_or("token exchange failed")
            .to_string();
        return Err(Error::Unauthorized { message: msg });
    }

    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Internal {
            message: "token response is missing access_token".to_string(),
            correlation_id: "login_token_response".to_string(),
        })?
        .to_string();
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let expires_in = json
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    Ok((access_token, refresh_token, expires_in))
}

#[cfg(test)]
mod tests;
