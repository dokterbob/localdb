//! OAuth2 authorization-code + PKCE HTTP surface (T4, specs/05-surfaces.md
//! §3.1): `GET/POST /authorize`, `POST /token`, `POST /revoke` (RFC 7009).
//!
//! HTTP shape only (specs/01-architecture.md §1) — the auth-code state
//! machine, client recognition, and redirect-uri policy all live in
//! `localdb_core::auth` (`AuthService::issue_auth_code`/`redeem_auth_code`,
//! `is_known_client`, `validate_redirect_uri`); persistence is
//! `store-libsql`'s `auth_codes` table. This module only: parses/renders
//! HTTP, escapes untrusted values into HTML, and maps `core::Error` onto
//! RFC 6749 §5.2 JSON error bodies.
//!
//! These three routes are deliberately **public** (no bearer token) — they
//! *are* the auth flow (specs/05-surfaces.md §3.1's route table). See
//! `daemon::build_router` for how they're kept outside the `require_auth`
//! layer.

use axum::{
    extract::{Form, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use localdb_core::auth::{self, AuthStore as _, Role};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Shared param validation (GET and POST /authorize)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    /// Pre-fills the consent form's credential field (D3b bootstrap UX) —
    /// the form still requires an explicit submit.
    pub setup_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeForm {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    /// Either the one-time setup code (bootstrap) or an existing API key —
    /// see `resolve_credential`. T6 adds a third branch here (invite
    /// tokens); that seam is `resolve_credential`, not this struct.
    pub credential: Option<String>,
}

/// The subset of `/authorize` params needed once validated: known-good
/// `client_id`/`redirect_uri`/`code_challenge`, plus the round-tripped
/// (untouched) CSRF `state`.
#[derive(Debug)]
struct ValidParams {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
}

#[derive(Debug)]
struct OAuthError {
    code: &'static str,
    description: String,
}

impl OAuthError {
    fn new(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }
}

fn validate_authorize_params(
    response_type: Option<&str>,
    client_id: Option<&str>,
    redirect_uri: Option<&str>,
    state: Option<&str>,
    code_challenge: Option<&str>,
    code_challenge_method: Option<&str>,
) -> Result<ValidParams, OAuthError> {
    if response_type.unwrap_or_default() != "code" {
        return Err(OAuthError::new(
            "unsupported_response_type",
            "response_type must be 'code'",
        ));
    }
    let client_id = client_id
        .filter(|s| !s.is_empty())
        .ok_or_else(|| OAuthError::new("invalid_request", "client_id is required"))?;
    if !auth::is_known_client(client_id) {
        return Err(OAuthError::new("unauthorized_client", "unknown client_id"));
    }
    let redirect_uri = redirect_uri
        .filter(|s| !s.is_empty())
        .ok_or_else(|| OAuthError::new("invalid_request", "redirect_uri is required"))?;
    if !auth::validate_redirect_uri(client_id, redirect_uri) {
        return Err(OAuthError::new(
            "invalid_request",
            "redirect_uri is not allowed for this client",
        ));
    }
    if code_challenge_method.unwrap_or_default() != "S256" {
        return Err(OAuthError::new(
            "invalid_request",
            "code_challenge_method must be 'S256' (plain is rejected)",
        ));
    }
    let code_challenge = code_challenge
        .filter(|s| !s.is_empty())
        .ok_or_else(|| OAuthError::new("invalid_request", "code_challenge is required"))?;

    Ok(ValidParams {
        client_id: client_id.to_string(),
        redirect_uri: redirect_uri.to_string(),
        state: state.unwrap_or_default().to_string(),
        code_challenge: code_challenge.to_string(),
    })
}

/// Escape a value for safe interpolation into the consent page HTML (XSS
/// hardening for `client_id`/`redirect_uri`/`state`, which are fully
/// attacker-controlled query/form params).
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A minimal, dependency-free (inline CSS only) consent page. Every
/// interpolated value is HTML-escaped. The hidden fields round-trip the
/// full set of OAuth params the POST handler re-validates.
fn render_consent_page(
    params: &ValidParams,
    credential_prefill: &str,
    error: Option<&str>,
) -> Html<String> {
    let client_id = escape_html(&params.client_id);
    let redirect_uri = escape_html(&params.redirect_uri);
    let state = escape_html(&params.state);
    let code_challenge = escape_html(&params.code_challenge);
    let credential_prefill = escape_html(credential_prefill);
    let error_html = match error {
        Some(e) => format!("<p class=\"error\">{}</p>", escape_html(e)),
        None => String::new(),
    };

    Html(format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>Authorize localdb</title>
<style>
body {{ font-family: -apple-system, system-ui, sans-serif; max-width: 420px; margin: 48px auto; padding: 0 16px; color: #1a1a1a; }}
h1 {{ font-size: 1.25rem; }}
label {{ display: block; margin-top: 16px; font-size: 0.9rem; }}
input[type="text"], input[type="password"] {{ width: 100%; padding: 8px; margin-top: 4px; box-sizing: border-box; font-size: 1rem; }}
button {{ margin-top: 20px; padding: 8px 20px; font-size: 1rem; }}
.error {{ color: #b00020; }}
.hint {{ color: #666; font-size: 0.85rem; }}
</style>
</head>
<body>
<h1>Authorize &ldquo;{client_id}&rdquo;</h1>
<p>This application is requesting access to your localdb data.</p>
{error_html}
<form method="post" action="/authorize">
<input type="hidden" name="response_type" value="code">
<input type="hidden" name="client_id" value="{client_id}">
<input type="hidden" name="redirect_uri" value="{redirect_uri}">
<input type="hidden" name="state" value="{state}">
<input type="hidden" name="code_challenge" value="{code_challenge}">
<input type="hidden" name="code_challenge_method" value="S256">
<label>One-time setup code (first run) or existing API key
<input type="password" name="credential" value="{credential_prefill}" autofocus></label>
<p class="hint">New installs: paste the setup code printed by <code>localdb serve</code>. Otherwise, use an API key from <code>localdb key create</code>.</p>
<button type="submit">Authorize</button>
</form>
</body>
</html>"#
    ))
}

fn oauth_error_page(err: OAuthError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html(format!(
            "<!doctype html><html><body><h1>Authorization error</h1><p><code>{}</code>: {}</p></body></html>",
            escape_html(err.code),
            escape_html(&err.description)
        )),
    )
        .into_response()
}

/// `GET /authorize` — renders the consent form. Params are validated before
/// rendering so a structurally invalid request (bad `redirect_uri`, missing
/// PKCE, unknown client) never gets a form that could plausibly complete.
pub async fn get_authorize(Query(query): Query<AuthorizeQuery>) -> Response {
    let params = match validate_authorize_params(
        query.response_type.as_deref(),
        query.client_id.as_deref(),
        query.redirect_uri.as_deref(),
        query.state.as_deref(),
        query.code_challenge.as_deref(),
        query.code_challenge_method.as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => return oauth_error_page(e),
    };
    let prefill = query.setup_code.unwrap_or_default();
    render_consent_page(&params, &prefill, None).into_response()
}

/// `POST /authorize` — validates the credential (setup code or API key),
/// issues a single-use authorization code, and redirects to `redirect_uri`
/// with `code` + the untouched `state` (or, for the `urn:ietf:wg:oauth:2.0:oob`
/// no-browser sentinel, renders the code inline for copy/paste instead of
/// redirecting — see `localdb_core::auth::OOB_REDIRECT_URI`).
pub async fn post_authorize(
    State(state): State<AppState>,
    Form(form): Form<AuthorizeForm>,
) -> Response {
    let params = match validate_authorize_params(
        form.response_type.as_deref(),
        form.client_id.as_deref(),
        form.redirect_uri.as_deref(),
        form.state.as_deref(),
        form.code_challenge.as_deref(),
        form.code_challenge_method.as_deref(),
    ) {
        Ok(p) => p,
        Err(e) => return oauth_error_page(e),
    };

    let credential = form.credential.unwrap_or_default();
    if credential.trim().is_empty() {
        return render_consent_page(&params, "", Some("Enter a setup code or API key."))
            .into_response();
    }

    let user_id = match resolve_credential(&state, credential.trim()).await {
        Ok(id) => id,
        Err(message) => return render_consent_page(&params, "", Some(&message)).into_response(),
    };

    let issued = match state
        .auth()
        .issue_auth_code(
            &params.client_id,
            &user_id,
            &params.redirect_uri,
            &params.code_challenge,
        )
        .await
    {
        Ok(issued) => issued,
        Err(e) => {
            return oauth_error_page(OAuthError::new("server_error", e.to_string()));
        }
    };

    if params.redirect_uri == auth::OOB_REDIRECT_URI {
        // No listener to redirect to (`localdb login --no-browser`): render
        // the code inline for the operator to copy and paste back into the
        // CLI's stdin prompt, rather than a 302 with nowhere to land.
        return render_oob_success_page(&issued.secret).into_response();
    }

    let redirect_to = append_query(
        &params.redirect_uri,
        &[
            ("code", issued.secret.as_str()),
            ("state", params.state.as_str()),
        ],
    );
    Redirect::to(&redirect_to).into_response()
}

/// The `localdb login --no-browser` success page: the code is displayed
/// (escaped) for the operator to copy and paste back into the CLI's stdin
/// prompt, since there is no listener to redirect to.
fn render_oob_success_page(code: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>localdb login code</title>
<style>body {{ font-family: -apple-system, system-ui, sans-serif; max-width: 420px; margin: 48px auto; padding: 0 16px; }}
code {{ font-size: 1.1rem; user-select: all; background: #f0f0f0; padding: 4px 8px; display: inline-block; margin-top: 8px; }}</style>
</head><body>
<h1>Authorization complete</h1>
<p>Copy this code and paste it back into the terminal running <code>localdb login --no-browser</code>:</p>
<p><code>{}</code></p>
</body></html>"#,
        escape_html(code)
    ))
}

/// Resolve a presented consent-form credential to a user id: try the
/// one-time setup code first (bootstrap path — creates the first admin user
/// and consumes the code), then fall back to treating it as an existing
/// bearer secret (API key or access token) via `AuthService::authenticate`.
///
/// T6 seam: an invite-token redemption branch belongs here, tried alongside
/// (not necessarily before/after) the API-key fallback — invites identify a
/// *new* user the way the setup code identifies the *first* one.
async fn resolve_credential(state: &AppState, credential: &str) -> Result<String, String> {
    let presented_hash = auth::hash_secret(credential);
    if state.consume_setup_code_if_matches(&presented_hash) {
        // Defense in depth: the setup code is only minted at startup when
        // `count_users() == 0`, but a user could have been created via a
        // different path (break-glass CLI) since then. Guard again here so
        // the bootstrap path can never create a second implicit admin.
        let existing = state
            .auth_store()
            .count_users()
            .await
            .map_err(|e| e.to_string())?;
        if existing > 0 {
            return Err(
                "setup code is no longer valid; an admin account already exists".to_string(),
            );
        }
        let user = state
            .auth()
            .create_user("admin", Role::Admin)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(user.id);
    }

    match state.auth().authenticate(credential).await {
        Ok(principal) => Ok(principal.user_id),
        Err(_) => Err("invalid setup code or API key".to_string()),
    }
}

/// Append `pairs` as query parameters to `redirect_uri`, preserving any
/// query string it already carries.
fn append_query(redirect_uri: &str, pairs: &[(&str, &str)]) -> String {
    match url::Url::parse(redirect_uri) {
        Ok(mut parsed) => {
            {
                let mut qp = parsed.query_pairs_mut();
                for (k, v) in pairs {
                    qp.append_pair(k, v);
                }
            }
            parsed.to_string()
        }
        // Unreachable in practice: `redirect_uri` was already validated by
        // `validate_authorize_params`/`validate_redirect_uri`, which only
        // accepts well-formed `http://` loopback URLs. Fall back to the raw
        // string rather than panicking if that invariant is ever violated.
        Err(_) => redirect_uri.to_string(),
    }
}

// ---------------------------------------------------------------------------
// POST /token
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    pub grant_type: Option<String>,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub client_id: Option<String>,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenErrorBody {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_description: Option<String>,
}

fn token_error(status: StatusCode, code: &'static str, description: impl Into<String>) -> Response {
    (
        status,
        Json(TokenErrorBody {
            error: code,
            error_description: Some(description.into()),
        }),
    )
        .into_response()
}

#[derive(Debug, Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
}

/// `POST /token` — `authorization_code` (+ `code_verifier`) and
/// `refresh_token` grants (specs/05-surfaces.md §3.1). RFC 6749 §5.2 JSON
/// error shape on failure, `400` (`401` for an unrecognized `client_id`).
pub async fn post_token(State(state): State<AppState>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_deref() {
        Some("authorization_code") => handle_auth_code_grant(&state, form).await,
        Some("refresh_token") => handle_refresh_grant(&state, form).await,
        Some(_) => token_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "grant_type must be 'authorization_code' or 'refresh_token'",
        ),
        None => token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "grant_type is required",
        ),
    }
}

async fn handle_auth_code_grant(state: &AppState, form: TokenForm) -> Response {
    let (Some(code), Some(redirect_uri), Some(client_id), Some(code_verifier)) = (
        form.code,
        form.redirect_uri,
        form.client_id,
        form.code_verifier,
    ) else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code, redirect_uri, client_id, and code_verifier are all required",
        );
    };
    if !auth::is_known_client(&client_id) {
        return token_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "unknown client_id",
        );
    }

    let user = match state
        .auth()
        .redeem_auth_code(&code, &client_id, &redirect_uri, &code_verifier)
        .await
    {
        Ok(user) => user,
        Err(_) => {
            return token_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the authorization code is invalid, expired, already used, or does not match \
                 this client/redirect_uri/code_verifier",
            )
        }
    };

    issue_token_pair(state, &user.id).await
}

async fn handle_refresh_grant(state: &AppState, form: TokenForm) -> Response {
    let Some(refresh_token) = form.refresh_token else {
        return token_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token is required",
        );
    };

    match state.auth().rotate_refresh_token(&refresh_token).await {
        Ok((access, refresh)) => Json(TokenResponse {
            access_token: access.secret,
            token_type: "Bearer",
            expires_in: localdb_core::auth::ACCESS_TOKEN_TTL_SECS,
            refresh_token: Some(refresh.secret),
        })
        .into_response(),
        Err(_) => token_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the refresh token is invalid, expired, or already used",
        ),
    }
}

async fn issue_token_pair(state: &AppState, user_id: &str) -> Response {
    let access = match state.auth().issue_access_token(user_id).await {
        Ok(a) => a,
        Err(e) => return token_error(StatusCode::BAD_REQUEST, "server_error", e.to_string()),
    };
    let refresh = match state.auth().issue_refresh_token(user_id).await {
        Ok(r) => r,
        Err(e) => return token_error(StatusCode::BAD_REQUEST, "server_error", e.to_string()),
    };
    Json(TokenResponse {
        access_token: access.secret,
        token_type: "Bearer",
        expires_in: localdb_core::auth::ACCESS_TOKEN_TTL_SECS,
        refresh_token: Some(refresh.secret),
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /revoke (RFC 7009)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RevokeForm {
    pub token: Option<String>,
    #[allow(dead_code)]
    pub token_type_hint: Option<String>,
}

/// `POST /revoke` — always `200`, even for an unknown token (RFC 7009 §2.2:
/// revocation must never leak whether a presented token existed).
pub async fn post_revoke(State(state): State<AppState>, Form(form): Form<RevokeForm>) -> Response {
    if let Some(token) = form.token.filter(|t| !t.is_empty()) {
        let _ = state.auth().revoke_by_secret(&token).await;
    }
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_html_neutralizes_script_tags() {
        let escaped = escape_html("<script>alert(1)</script>");
        assert!(!escaped.contains("<script>"));
        assert!(escaped.contains("&lt;script&gt;"));
    }

    #[test]
    fn validate_authorize_params_happy_path() {
        let params = validate_authorize_params(
            Some("code"),
            Some("localdb-cli"),
            Some("http://127.0.0.1:1234/callback"),
            Some("xyz"),
            Some("challenge-value"),
            Some("S256"),
        )
        .unwrap();
        assert_eq!(params.client_id, "localdb-cli");
        assert_eq!(params.state, "xyz");
    }

    #[test]
    fn validate_authorize_params_rejects_bad_redirect_uri() {
        let err = validate_authorize_params(
            Some("code"),
            Some("localdb-cli"),
            Some("http://evil.example.com/callback"),
            None,
            Some("c"),
            Some("S256"),
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn validate_authorize_params_rejects_missing_pkce() {
        let err = validate_authorize_params(
            Some("code"),
            Some("localdb-cli"),
            Some("http://127.0.0.1:1/callback"),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn validate_authorize_params_rejects_plain_challenge_method() {
        let err = validate_authorize_params(
            Some("code"),
            Some("localdb-cli"),
            Some("http://127.0.0.1:1/callback"),
            None,
            Some("c"),
            Some("plain"),
        )
        .unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn validate_authorize_params_rejects_unknown_client() {
        let err = validate_authorize_params(
            Some("code"),
            Some("some-other-client"),
            Some("http://127.0.0.1:1/callback"),
            None,
            Some("c"),
            Some("S256"),
        )
        .unwrap_err();
        assert_eq!(err.code, "unauthorized_client");
    }

    #[test]
    fn consent_page_escapes_hostile_state_and_client_id() {
        let params = ValidParams {
            client_id: "<script>alert(1)</script>".to_string(),
            redirect_uri: "http://127.0.0.1:1/cb".to_string(),
            state: "\"><script>alert(2)</script>".to_string(),
            code_challenge: "c".to_string(),
        };
        let html = render_consent_page(&params, "", None).0;
        assert!(
            !html.contains("<script>"),
            "raw script tag must never appear: {html}"
        );
    }

    #[test]
    fn append_query_preserves_existing_query_string() {
        let out = append_query(
            "http://127.0.0.1:1234/callback?foo=bar",
            &[("code", "abc"), ("state", "xyz")],
        );
        assert!(out.contains("foo=bar"));
        assert!(out.contains("code=abc"));
        assert!(out.contains("state=xyz"));
    }
}
