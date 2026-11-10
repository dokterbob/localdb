//! The standalone invite-join page (`GET/POST /join`, specs/05-surfaces.md
//! §3.1.1): a browser flow for an invited person with no OAuth client and no
//! CLI running. It redeems an invite the same way the OAuth consent page's
//! invite branch does (`super::oauth::handle_invite_authorize`), but ends in
//! a page of its own rather than resuming an OAuth authorization, since
//! there is no client waiting for a redirect.
//!
//! HTTP shape only (specs/01-architecture.md §1): invite redemption and
//! polling state live in `localdb_core::auth::AuthService`.

use axum::{
    extract::{Form, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;

use localdb_core::auth::RedeemOutcome;

use super::oauth::escape_html;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct JoinQuery {
    pub invite: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct JoinForm {
    pub invite: Option<String>,
    pub requested_name: Option<String>,
}

const PAGE_STYLE: &str = r#"
body { font-family: -apple-system, system-ui, sans-serif; max-width: 420px; margin: 48px auto; padding: 0 16px; color: #1a1a1a; }
h1 { font-size: 1.25rem; }
label { display: block; margin-top: 16px; font-size: 0.9rem; }
input[type="text"] { width: 100%; padding: 8px; margin-top: 4px; box-sizing: border-box; font-size: 1rem; }
button { margin-top: 20px; padding: 8px 20px; font-size: 1rem; }
.error { color: #b00020; }
.hint { color: #666; font-size: 0.85rem; }
ul { padding-left: 20px; }
code { font-size: 1.05rem; user-select: all; background: #f0f0f0; padding: 4px 8px; display: inline-block; margin-top: 8px; word-break: break-all; }
"#;

/// Wraps `body` in a minimal, dependency-free (inline CSS only) HTML
/// document. The emitted `<body>` tag carries no attributes, so a caller that
/// needs to add its own (the waiting page's `data-*` attributes) can do so
/// with a string replace, the same technique
/// `oauth::render_request_submitted_page` uses on the consent page.
fn page_shell(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>{title}</title>
<style>
{PAGE_STYLE}
</style>
</head>
<body>
{body}
</body>
</html>"#
    ))
}

fn missing_invite_error() -> Response {
    (
        StatusCode::BAD_REQUEST,
        page_shell(
            "Join localdb",
            "<h1>Join localdb</h1><p class=\"error\">Missing or empty invite token.</p>",
        ),
    )
        .into_response()
}

/// The join form: a hidden `invite` field and a "Your name" input, posting
/// back to this same page.
fn render_join_form(invite_token: &str, error: Option<&str>) -> Html<String> {
    let token_escaped = escape_html(invite_token);
    let error_html = match error {
        Some(e) => format!("<p class=\"error\">{}</p>", escape_html(e)),
        None => String::new(),
    };
    page_shell(
        "Join localdb",
        &format!(
            r#"<h1>Join localdb</h1>
<p>You were invited to join this localdb instance. Choose a name to continue.</p>
{error_html}
<form method="post" action="">
<input type="hidden" name="invite" value="{token_escaped}">
<label>Your name
<input type="text" name="requested_name" autofocus></label>
<button type="submit">Join</button>
</form>"#
        ),
    )
}

/// The show-once success page for an `open`-mode invite: the new API key,
/// shown exactly once, plus the stores it grants and how to use it.
fn render_joined_page(name: &str, api_key: &str, grants: &[String]) -> Html<String> {
    let name_escaped = escape_html(name);
    let key_escaped = escape_html(api_key);
    let stores_html = if grants.is_empty() {
        "<p>No stores were granted by this invite.</p>".to_string()
    } else {
        let items: String = grants
            .iter()
            .map(|g| format!("<li>{}</li>", escape_html(g)))
            .collect();
        format!("<p>Granted stores:</p><ul>{items}</ul>")
    };
    page_shell(
        "Joined localdb",
        &format!(
            r#"<h1>Joined as {name_escaped}</h1>
<p>Your API key (shown once — store it now, it cannot be recovered):</p>
<code>{key_escaped}</code>
{stores_html}
<p class="hint">Paste it at the consent page your MCP client opens, or set <code>LOCALDB_API_KEY</code>.</p>"#
        ),
    )
}

/// The waiting page for a `closed`-mode invite: the same polling mechanics as
/// the OAuth consent page's own waiting page
/// (`oauth::render_request_submitted_page`), but `data-on-approve="show"`
/// tells the shared script in `invite_wait.html` to display the collected
/// API key here instead of submitting a hidden OAuth form — there is no
/// OAuth flow to resume. `data-poll-url` is a relative path, so it keeps
/// working behind any path prefix the daemon is mounted under.
fn render_join_wait_page(request_id: &str, request_secret: &str) -> Html<String> {
    let page = page_shell("Join localdb", "<h1>Join localdb</h1>").0;
    let poll_url = escape_html(&format!("v1/invites/requests/{request_id}/poll"));
    let body_tag = format!(
        "<body data-request-id=\"{}\" data-request-secret=\"{}\" data-poll-url=\"{}\" \
         data-on-approve=\"show\">",
        escape_html(request_id),
        escape_html(request_secret),
        poll_url,
    );
    Html(
        page.replace("<body>", &body_tag)
            .replace("</body>", include_str!("invite_wait.html")),
    )
}

/// `GET /join?invite=<token>`: renders the join form, or a 400 error page
/// when `invite` is missing or empty.
pub async fn get_join(Query(query): Query<JoinQuery>) -> Response {
    match query.invite.as_deref().filter(|s| !s.is_empty()) {
        Some(token) => render_join_form(token, None).into_response(),
        None => missing_invite_error(),
    }
}

/// `POST /join` (form: `invite`, `requested_name`): redeems the invite.
/// `open` mode ends in a show-once success page; `closed` mode ends in a
/// waiting page that polls for admin approval; an empty name or a redemption
/// failure re-renders the form with the error, the same convention
/// `oauth::handle_invite_authorize` uses.
pub async fn post_join(State(state): State<AppState>, Form(form): Form<JoinForm>) -> Response {
    let Some(invite_token) = form.invite.as_deref().filter(|s| !s.is_empty()) else {
        return missing_invite_error();
    };
    let requested_name = form.requested_name.unwrap_or_default();
    let requested_name = requested_name.trim();
    if requested_name.is_empty() {
        return render_join_form(invite_token, Some("Enter your name.")).into_response();
    }
    match state
        .auth()
        .redeem_invite(invite_token, requested_name)
        .await
    {
        Ok(RedeemOutcome::Open {
            grants, credential, ..
        }) => render_joined_page(requested_name, &credential.secret, &grants).into_response(),
        Ok(RedeemOutcome::Closed {
            request_id,
            request_secret,
        }) => render_join_wait_page(&request_id, &request_secret).into_response(),
        Err(e) => render_join_form(invite_token, Some(&e.to_string())).into_response(),
    }
}

#[cfg(test)]
mod tests;
