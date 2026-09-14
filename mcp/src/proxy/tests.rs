use super::*;

fn test_scope(stores: &[(&str, &str)], allowed: &[&str]) -> ProxyScope {
    ProxyScope {
        upstream_stores: stores
            .iter()
            .map(|(id, name)| UpstreamStore {
                id: id.to_string(),
                name: name.to_string(),
            })
            .collect(),
        allowed_ids: allowed.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn canonicalize_accepts_in_scope_name_and_returns_its_id() {
    let scope = test_scope(
        &[("id-books", "books"), ("id-hydra", "hydra")],
        &["id-books"],
    );
    assert_eq!(scope.canonicalize("books").unwrap(), "id-books");
}

#[test]
fn canonicalize_accepts_in_scope_id_verbatim() {
    // #144's citation round-trip passes `store.id`, not the name.
    let scope = test_scope(
        &[("id-books", "books"), ("id-hydra", "hydra")],
        &["id-books"],
    );
    assert_eq!(scope.canonicalize("id-books").unwrap(), "id-books");
}

#[test]
fn canonicalize_rejects_out_of_scope_store_by_name_and_by_id() {
    let scope = test_scope(
        &[("id-books", "books"), ("id-hydra", "hydra")],
        &["id-books"],
    );
    assert!(scope.canonicalize("hydra").is_err());
    assert!(scope.canonicalize("id-hydra").is_err());
}

#[test]
fn canonicalize_rejects_a_name_no_store_has() {
    let scope = test_scope(&[("id-books", "books")], &["id-books"]);
    assert!(scope.canonicalize("nonexistent").is_err());
}

/// The shadowing case: the caller passes a value that is an out-of-scope
/// store's **id** and simultaneously an in-scope store's **name**. The
/// upstream resolves ids first, so approving this on the name match would
/// hand back the out-of-scope store's data. Membership must therefore be
/// judged on what the *upstream* would resolve, not on the allowed subset.
#[test]
fn canonicalize_rejects_in_scope_name_shadowing_an_out_of_scope_id() {
    let shadow = "shadow-value";
    let scope = test_scope(
        // `id-books` is named `shadow-value`; `shadow-value` is *also*
        // the id of the out-of-scope `hydra`.
        &[("id-books", shadow), (shadow, "hydra")],
        &["id-books"],
    );
    assert!(
        scope.canonicalize(shadow).is_err(),
        "a value that resolves (id-first) to an out-of-scope store must be rejected \
         even though it also matches an in-scope store's name"
    );
}

#[test]
fn allowed_names_lists_only_in_scope_stores() {
    let scope = test_scope(
        &[("id-a", "alpha"), ("id-b", "beta"), ("id-c", "gamma")],
        &["id-a", "id-c"],
    );
    assert_eq!(scope.allowed_names(), "alpha, gamma");
}

#[test]
fn scope_rejection_is_tool_level_invalid_request_naming_the_allowed_set() {
    let result = scope_rejection("hydra", "books");
    assert_eq!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["error"]["code"], "invalid_request");
    let message = parsed["error"]["message"].as_str().unwrap();
    assert!(message.contains("hydra"), "{message}");
    assert!(message.contains("books"), "{message}");
}
