//! `localdb login` / `logout` (specs/05-surfaces.md §2, §3.1).
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
    daemon_client::{
        build_http_client, encode_path_segment, probe_daemon, resolved_config_file, CliContext,
        DaemonState,
    },
    normalize::{exit_err, print_json},
};

mod callback;

const CLIENT_ID: &str = "localdb-cli";

/// `localdb login [--url <base>] [--setup-code <code>] [--no-browser]
/// [--invite <token>] [--name <name>]`
///
/// `--invite` takes an entirely different, browser-free path
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
    // Run the credentials-file preflight before resolving the base URL:
    // `resolve_login_base_url` can itself fail (`Error::DaemonUnreachable`
    // when no `--url` was given and no daemon is reachable) or have a side
    // effect (cleaning up stale discovery files), and a malformed
    // `credentials.json` must win over either of those rather than being
    // masked by them. `perform_login`/`perform_invite_login` keep their own
    // preflight call too, since each is also reachable (and tested)
    // independently of this entry point.
    if let Err(e) = credentials_file_ready(ctx) {
        exit_err(&e, ctx.json);
    }

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
            Ok((_joined_name, bearer)) => report_login_success(ctx, &base_url, &bearer).await,
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
        Ok(bearer) => report_login_success(ctx, &base_url, &bearer).await,
        Err(e) => exit_err(&e, ctx.json),
    }
}

/// Report a completed login: look up the caller's resolved identity via
/// `GET /v1/auth/me`, using `bearer` — the credential the login flow that
/// just ran (OAuth or invite) obtained directly, not re-resolved through
/// `fresh_bearer_for_url`/`ctx.api_key`, which could belong to an entirely
/// different user (a `LOCALDB_API_KEY`/`--api-key` left over from another
/// session) and would then report the wrong identity, or fail, regardless
/// of whether the login that just ran succeeded. Shared by both the OAuth
/// and invite login paths, so the two report identically rather than the
/// invite path only ever knowing the name it asked for (not the daemon's
/// actual, possibly different, resolved role).
///
/// The identity lookup is best-effort: by the time this runs, the
/// credential is already persisted, so a failure here (timeout, error
/// status, malformed body) must not turn a successful login into a
/// non-zero exit — that would make automation re-run a login whose side
/// effect (e.g. consuming a single-use invite) already happened.
async fn report_login_success(ctx: &CliContext, base_url: &str, bearer: &str) {
    let result = build_login_result(ctx, base_url, bearer).await;
    print_login_result(ctx, &result);
}

/// The outcome of a completed login: the same `base_url`/`name`/`role`
/// fields both the `--json` envelope and the human-readable confirmation
/// use. Built once by [`build_login_result`] so a test can assert on
/// `name`/`role` by calling it directly, rather than by capturing stdout
/// or driving a process exit.
struct LoginResult {
    base_url: String,
    name: Option<String>,
    role: Option<String>,
}

/// Resolve the identity behind a completed login and build the result that
/// gets printed. `bearer` is the credential the login flow that just ran
/// (OAuth or invite — both call sites in `run_login_async` route through
/// this one function) obtained directly; it is never re-resolved through
/// `ctx`/`credentials.json`, which could hold an entirely different
/// credential (e.g. a `LOCALDB_API_KEY` left over from another session)
/// and would then report the wrong identity regardless of who just logged
/// in.
async fn build_login_result(_ctx: &CliContext, base_url: &str, bearer: &str) -> LoginResult {
    let identity = resolve_identity_best_effort(base_url, bearer).await;
    LoginResult {
        base_url: base_url.to_string(),
        name: identity.as_ref().map(|(name, _)| name.clone()),
        role: identity.as_ref().map(|(_, role)| role.clone()),
    }
}

/// Look up the identity behind `bearer`, warning on stderr and returning
/// `None` instead of propagating an error — see [`build_login_result`]
/// for why a failure here must not be fatal.
async fn resolve_identity_best_effort(base_url: &str, bearer: &str) -> Option<(String, String)> {
    match fetch_identity(base_url, bearer).await {
        Ok(identity) => Some(identity),
        Err(e) => {
            eprintln!("warning: login succeeded but could not resolve the logged-in identity: {e}");
            None
        }
    }
}

/// Print/emit a completed login's result. With no resolved identity (the
/// lookup failed), `--json` carries explicit `null`s and human output
/// falls back to a plain confirmation rather than naming a user or role it
/// couldn't confirm.
fn print_login_result(ctx: &CliContext, result: &LoginResult) {
    if ctx.json {
        print_json(&json!({
            "status": "ok",
            "base_url": result.base_url,
            "name": result.name,
            "role": result.role,
        }));
    } else {
        match (&result.name, &result.role) {
            (Some(name), Some(role)) => {
                println!("Logged in to {} as '{name}' ({role}).", result.base_url)
            }
            _ => println!("Logged in to {}.", result.base_url),
        }
    }
}

/// `GET /v1/auth/me` with the given bearer, returning `(name, role)`.
async fn fetch_identity(base_url: &str, bearer: &str) -> Result<(String, String), Error> {
    let client = build_http_client(std::time::Duration::from_secs(10))?;
    let resp = client
        .get(format!("{base_url}/v1/auth/me"))
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(|_| Error::DaemonUnreachable)?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        return Err(crate::daemon_client::daemon_response_error(status, &body));
    }
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Internal {
            message: "auth/me response is missing name".to_string(),
            correlation_id: "login_identity_response".to_string(),
        })?
        .to_string();
    let role = body
        .get("role")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Internal {
            message: "auth/me response is missing role".to_string(),
            correlation_id: "login_identity_response".to_string(),
        })?
        .to_string();
    Ok((name, role))
}

/// Resolve `credentials.json`'s path for `ctx` and confirm it can be
/// loaded for a write: a missing file is fine (the first write creates
/// it), but an unreadable or malformed one fails here, before either login
/// flow (browser/OOB or `--invite`) starts. Both flows have an irreversible
/// side effect ahead of their final write — the browser/OOB flow consumes a
/// single-use authorization code (and, for the first admin, a one-time
/// bootstrap setup code), the invite flow consumes an invite's one-time
/// use — so finding out the file is unusable only at that final write burns
/// a credential that can never be un-consumed. The returned error never
/// quotes the file's contents (see `credentials::describe_parse_error`).
fn credentials_file_ready(ctx: &CliContext) -> Result<std::path::PathBuf, Error> {
    let config_file = resolved_config_file(ctx).ok_or_else(|| Error::InvalidConfig {
        message: "cannot resolve the config file path to write credentials.json".to_string(),
    })?;
    let credentials_file = crate::credentials::credentials_path(&config_file);
    crate::credentials::ensure_loadable_for_write(&credentials_file).map_err(|e| {
        Error::InvalidConfig {
            message: format!("{e}; fix or remove the file before logging in"),
        }
    })?;
    Ok(credentials_file)
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

/// `localdb login --invite <token>` redeems the invite directly
/// against `POST /v1/invites/redeem` — no browser round trip needed. This
/// is the *primary* closed-mode path (the consent page's invite branch,
/// `server::auth::oauth::handle_invite_authorize`, exists too, but only for
/// the browser flow; it can't drive a poll loop the way this can). `open`
/// mode: one request, done. `closed` mode: polls
/// `POST /v1/invites/requests/{id}/poll` every `poll_interval` until an
/// admin decides. `poll_interval` is injected (production: 1s) so tests can
/// drive the loop with a much shorter interval instead of a multi-second
/// sleep. Returns the requested name together with the minted API key, so
/// the caller can use that exact credential for an immediate follow-up
/// request instead of re-resolving a bearer from `ctx`.
async fn perform_invite_login(
    ctx: &CliContext,
    base_url: &str,
    invite_token: &str,
    name: Option<&str>,
    poll_interval: std::time::Duration,
) -> Result<(String, String), Error> {
    let requested_name = resolve_requested_name(name)?;
    credentials_file_ready(ctx)?;

    let client = build_http_client(std::time::Duration::from_secs(30))?;

    let resp = client
        .post(format!("{base_url}/v1/invites/redeem"))
        .json(&json!({ "token": invite_token, "requested_name": requested_name }))
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
            })?
            .to_string();
        persist_api_key(ctx, base_url, &api_key).await?;
        return Ok((requested_name, api_key));
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
        return Ok((requested_name, api_key));
    }

    Err(crate::daemon_client::daemon_response_error(status, &body))
}

/// Upper bound on how long a single `Retry-After`-driven wait may stretch
/// this poll loop, regardless of what value the header carries — a
/// hostile or merely broken daemon must not be able to stall `login`
/// indefinitely with one absurd value (e.g. `Retry-After: 315360000`).
const MAX_RATE_LIMIT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// How many consecutive `429`s this loop tolerates before giving up.
/// Waiting for a human's approval is legitimately open-ended, so this loop
/// has no overall deadline on `pending` — but a `429` means this loop
/// itself is being throttled, not that the request is still undecided, and
/// repeated `429`s must not be able to extend the wait without bound
/// either (a `Retry-After: 0`, honored literally, would otherwise hot-loop
/// forever just as easily as a huge one stalls forever). After this many
/// in a row — each one waited out in full — the loop gives up with a
/// clear, retryable error instead of continuing forever.
const MAX_CONSECUTIVE_RATE_LIMITS: u32 = 20;

/// Poll `POST /v1/invites/requests/{id}/poll` every `poll_interval` until
/// the request is decided. Returns the credential on approval; `denied` is
/// a terminal `Unauthorized` with a clear message, per specs/05-surfaces.md
/// §2. The secret is sent in the JSON body, never the URL — see
/// `server::handlers::invites::poll_request`'s doc comment for why.
async fn poll_until_decided(
    client: &reqwest::Client,
    base_url: &str,
    request_id: &str,
    request_secret: &str,
    poll_interval: std::time::Duration,
) -> Result<String, Error> {
    let encoded_id = encode_path_segment(request_id);
    let mut consecutive_rate_limits = 0u32;
    loop {
        let resp = client
            .post(format!("{base_url}/v1/invites/requests/{encoded_id}/poll"))
            .json(&json!({ "secret": request_secret }))
            .send()
            .await
            .map_err(|_| Error::DaemonUnreachable)?;
        let status = resp.status();
        // A 429 from the daemon's per-IP rate limiter (specs/05-surfaces.md
        // §3.1) is not a decision failure — it means this poll loop itself
        // is the thing being throttled (e.g. several concurrent logins
        // sharing one IP behind NAT). Wait the suggested delay, clamped to
        // a sane range, and keep polling rather than surfacing an error for
        // a condition this loop caused and can resolve by itself — unless
        // it keeps happening (see `MAX_CONSECUTIVE_RATE_LIMITS`).
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            consecutive_rate_limits += 1;
            if consecutive_rate_limits >= MAX_CONSECUTIVE_RATE_LIMITS {
                return Err(Error::RateLimited {
                    message: format!(
                        "the daemon rate-limited this poll {consecutive_rate_limits} times in a \
                         row; retry login later"
                    ),
                });
            }
            // The minimum is `poll_interval` itself (never busy-loop faster
            // than this login would poll anyway, even on `Retry-After: 0`
            // or a missing header); the maximum is `MAX_RATE_LIMIT_WAIT`
            // (never one absurd value away from a login that looks hung).
            let wait = retry_after_duration(resp.headers())
                .unwrap_or(poll_interval)
                .clamp(poll_interval, MAX_RATE_LIMIT_WAIT);
            tokio::time::sleep(wait).await;
            continue;
        }
        consecutive_rate_limits = 0;
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

/// Parse a `Retry-After` header as a plain integer number of seconds (the
/// only form this daemon's rate limiter sends — `server::auth::rate_limit`).
/// `None` for a missing or unparseable header, so the caller can fall back
/// to its own poll interval rather than fail outright on a header shape
/// this server never actually sends. The returned duration is not yet
/// clamped — callers apply their own bound (see `MAX_RATE_LIMIT_WAIT`).
fn retry_after_duration(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
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
        let client = build_http_client(std::time::Duration::from_secs(10))?;
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

/// Drive the authorization-code + PKCE flow end to end, returning the
/// access token it just obtained (and persisted) so the caller can use
/// that exact bearer for an immediate follow-up request, such as the
/// identity lookup in `report_login_success`, instead of re-resolving a
/// bearer from `ctx`/`credentials.json`, which could pick a different
/// credential (e.g. an `LOCALDB_API_KEY` left in the environment).
/// `open_browser` is injected so tests can substitute a fake "browser" (an
/// HTTP client that submits the consent form) instead of actually
/// launching one; production code passes `|u| webbrowser::open(u).is_ok()`.
/// `callback_timeout` bounds how long we wait for the loopback callback
/// (production: 5 minutes for a human to click through; tests use a much
/// shorter bound so a never-arriving callback — e.g. a rejected credential
/// — fails fast instead of stalling the suite).
async fn perform_login(
    ctx: &CliContext,
    base_url: &str,
    setup_code: Option<&str>,
    no_browser: bool,
    open_browser: impl Fn(&str) -> bool,
    callback_timeout: std::time::Duration,
) -> Result<String, Error> {
    let credentials_file = credentials_file_ready(ctx)?;

    let (verifier, challenge) = localdb_core::auth::generate_pkce_pair();
    let (csrf_state, _) = localdb_core::auth::generate_pkce_pair();

    let (redirect_uri, code) = if no_browser {
        let redirect_uri = localdb_core::auth::OOB_REDIRECT_URI.to_string();
        let authorize_url =
            build_authorize_url(base_url, &redirect_uri, &csrf_state, &challenge, setup_code);
        // Instructions and the paste prompt go to stderr, never stdout:
        // `--json` guarantees stdout is pure JSON, and these apply
        // regardless of `--json` since they are not part of the result.
        eprintln!(
            "Open this URL in a browser to authorize `localdb login`:\n\n  {authorize_url}\n"
        );
        eprint!("Paste the code shown after authorizing: ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| Error::Internal {
                message: format!("failed to read the pasted code from stdin: {e}"),
                correlation_id: "login_stdin".to_string(),
            })?;
        (redirect_uri, line.trim().to_string())
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
            eprintln!(
                "Could not open a browser automatically. On a remote or headless session, \
                 re-run with --no-browser. Otherwise open this URL on this machine:\n\n  \
                 {authorize_url}\n"
            );
        }

        // `callback::wait_for_callback` enforces the `state` match itself —
        // it only ever returns once a callback carrying exactly
        // `csrf_state` arrives (or the deadline passes), so there is
        // nothing left for this function to re-check.
        let code = callback::wait_for_callback(listener, &csrf_state, callback_timeout).await?;
        (redirect_uri, code)
    };

    if code.is_empty() {
        return Err(Error::InvalidRequest {
            message: "no authorization code was received".to_string(),
        });
    }

    let (access_token, refresh_token, expires_in) =
        exchange_code_for_tokens(base_url, &code, &redirect_uri, &verifier).await?;

    let entry = CredentialEntry {
        secret: None,
        access_token: Some(access_token.clone()),
        refresh_token,
        access_expires_at: Some(localdb_core::auth::rfc3339_from_now(expires_in)),
    };
    write_credential(&credentials_file, base_url, entry)
        .await
        .map_err(|e| Error::Internal {
            message: format!("failed to write credentials.json: {e}"),
            correlation_id: "login_persist_write".to_string(),
        })?;

    Ok(access_token)
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

async fn exchange_code_for_tokens(
    base_url: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<(String, Option<String>, i64), Error> {
    let client = build_http_client(std::time::Duration::from_secs(30))?;
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
        return Err(classify_token_error(status, msg));
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

/// Classify an unsuccessful `/token` response by HTTP status, rather than
/// collapsing every failure into `Unauthorized`: a `5xx` means the daemon
/// itself broke while handling the exchange — the caller's credentials were
/// never actually judged — so it is reported as an internal failure, not a
/// rejected login. Every other status (RFC 6749's `/token` errors —
/// `invalid_grant`, `invalid_client`, etc. — are all `4xx`) is a genuine
/// authentication failure.
fn classify_token_error(status: reqwest::StatusCode, message: String) -> Error {
    if status.is_server_error() {
        Error::Internal {
            message: format!(
                "daemon's /token endpoint returned {}: {message}",
                status.as_u16()
            ),
            correlation_id: "login_token_server_error".to_string(),
        }
    } else {
        Error::Unauthorized { message }
    }
}

#[cfg(test)]
mod tests;
