use super::*;

#[test]
fn join_form_escapes_hostile_invite_token() {
    let html = render_join_form("<script>alert(1)</script>", None).0;
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("&lt;script&gt;"));
}

#[test]
fn join_form_shows_name_error() {
    let html = render_join_form("tok", Some("Enter your name.")).0;
    assert!(html.contains("Enter your name."));
    assert!(html.contains(r#"name="requested_name""#));
}

#[test]
fn join_form_posts_to_the_current_path() {
    let html = render_join_form("tok", None).0;
    assert!(html.contains(r#"<form method="post" action="">"#));
}

#[test]
fn missing_invite_error_is_a_400_html_page() {
    let response = missing_invite_error();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn joined_page_escapes_hostile_name_and_shows_the_key_once() {
    let html = render_joined_page("<b>evil</b>", "ldb_secret123", &["docs".to_string()]).0;
    assert!(!html.contains("<b>evil</b>"));
    assert!(html.contains("&lt;b&gt;evil&lt;/b&gt;"));
    let occurrences = html.matches("ldb_secret123").count();
    assert_eq!(
        occurrences, 1,
        "the API key must appear exactly once: {html}"
    );
    assert!(html.contains("docs"));
}

#[test]
fn joined_page_with_no_grants_says_so() {
    let html = render_joined_page("newbie", "ldb_key", &[]).0;
    assert!(html.to_lowercase().contains("no stores"));
}

#[test]
fn joined_page_escapes_a_hostile_store_name() {
    let html = render_joined_page(
        "newbie",
        "ldb_key",
        &["<script>alert(1)</script>".to_string()],
    )
    .0;
    assert!(!html.contains("<script>alert(1)</script>"));
}

#[test]
fn join_wait_page_carries_a_relative_poll_url_and_show_mode() {
    let html = render_join_wait_page("req1", "sekret").0;
    assert!(html.contains(r#"data-on-approve="show""#));
    assert!(html.contains(r#"data-poll-url="v1/invites/requests/req1?secret=sekret""#));
    assert!(html.contains(r#"id="api-key""#));
}

#[test]
fn join_wait_page_escapes_hostile_request_id() {
    let html = render_join_wait_page("\"><script>alert(1)</script>", "sekret").0;
    assert!(!html.contains("<script>alert(1)</script>"));
}
