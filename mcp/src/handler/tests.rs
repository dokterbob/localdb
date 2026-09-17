use super::*;
use crate::store_provider::StaticStoreProvider;
use async_trait::async_trait;
use localdb_core::auth::{Role, StoreAccess};
use localdb_core::embedder::FakeEmbedder;

/// A `StoreProvider` that always fails, to prove a provider error
/// surfaces as a tool-level MCP error (not a panic) and the handler
/// remains usable afterward.
struct ErrorStoreProvider;

#[async_trait]
impl StoreProvider for ErrorStoreProvider {
    async fn available_stores(&self) -> Result<Vec<AvailableStore>, Error> {
        Err(Error::Internal {
            message: "backend unavailable".to_string(),
            correlation_id: "test_provider_error".to_string(),
        })
    }
}

fn embedder() -> Arc<dyn Embedder> {
    Arc::new(FakeEmbedder::new(4))
}

/// Empty rmcp extensions — what an embedded stdio call looks like (no
/// HTTP `Parts`, no injected `Principal`).
fn no_extensions() -> Extensions {
    Extensions::default()
}

/// Extensions carrying `principal` the way the daemon's HTTP path does:
/// inside the request extensions of the `http::request::Parts` rmcp
/// injects into `RequestContext.extensions`.
fn http_extensions_with_principal(principal: Principal) -> Extensions {
    let mut extensions = Extensions::default();
    let (mut parts, _body) = http::Request::new(()).into_parts();
    parts.extensions.insert(principal);
    extensions.insert(parts);
    extensions
}

fn member(name: &str) -> Principal {
    Principal {
        user_id: format!("uid-{name}"),
        name: name.to_string(),
        role: Role::Member,
        access: StoreAccess::Granted(Default::default()),
    }
}

fn error_code(result: &CallToolResult) -> String {
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
    parsed["error"]["code"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn list_stores_returns_tool_error_when_provider_fails() {
    let handler = McpHandler::new(
        Arc::new(ErrorStoreProvider),
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        Some(Principal::local_trust()),
    );
    let result = handler.list_stores_inner(&no_extensions()).await;
    assert_eq!(result.is_error, Some(true));
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("must be JSON");
    assert_eq!(parsed["error"]["code"].as_str().unwrap(), "internal");
    assert!(parsed["error"]["message"]
        .as_str()
        .unwrap()
        .contains("backend unavailable"));
}

#[tokio::test]
async fn handler_stays_usable_after_a_provider_error() {
    let handler = McpHandler::new(
        Arc::new(ErrorStoreProvider),
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        Some(Principal::local_trust()),
    );

    let first = handler.list_stores_inner(&no_extensions()).await;
    assert_eq!(first.is_error, Some(true));

    // The service must not have panicked or otherwise become unusable —
    // a second call against the same (still-failing) provider still
    // produces a clean tool-level error, not a crash.
    let second = handler.list_stores_inner(&no_extensions()).await;
    assert_eq!(second.is_error, Some(true));
}

#[tokio::test]
async fn list_stores_succeeds_once_provider_recovers() {
    // Same handler *shape* as the static case — proves a working
    // provider still round-trips normally through `resolve_stores`.
    let provider = Arc::new(StaticStoreProvider::new(vec![]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        Some(Principal::local_trust()),
    );
    let result = handler.list_stores_inner(&no_extensions()).await;
    assert_ne!(result.is_error, Some(true));
}

// --- Principal resolution (R4, fail-closed) ---

#[tokio::test]
async fn no_principal_and_no_default_fails_closed() {
    // Enforced-HTTP construction: default_principal = None. A context
    // with no injected Principal must produce an unauthorized tool
    // error — never full access.
    let provider = Arc::new(StaticStoreProvider::new(vec![]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let result = handler.list_stores_inner(&no_extensions()).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(error_code(&result), "unauthorized");
}

#[tokio::test]
async fn http_injected_admin_principal_is_used() {
    // Even with no default (enforced mode), a Principal delivered via
    // http::request::Parts extensions authorizes the call.
    let provider = Arc::new(StaticStoreProvider::new(vec![]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let result = handler
        .list_stores_inner(&http_extensions_with_principal(Principal::local_trust()))
        .await;
    assert_ne!(result.is_error, Some(true));
}

#[tokio::test]
async fn member_principal_is_no_longer_rejected_wholesale() {
    // T5 lifts the T3 interim member-403 block: a member now passes
    // (against an empty store set the call trivially succeeds with no
    // stores, rather than erroring).
    let provider = Arc::new(StaticStoreProvider::new(vec![]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let result = handler
        .list_stores_inner(&http_extensions_with_principal(member("bob")))
        .await;
    assert_ne!(result.is_error, Some(true));
}

#[tokio::test]
async fn default_local_trust_authorizes_embedded_calls() {
    // Embedded stdio: no HTTP Parts at all; default local_trust applies.
    let provider = Arc::new(StaticStoreProvider::new(vec![]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        Some(Principal::local_trust()),
    );
    let result = handler.list_stores_inner(&no_extensions()).await;
    assert_ne!(result.is_error, Some(true));
}

#[tokio::test]
async fn member_default_principal_is_no_longer_gated_either() {
    // Nothing constructs this today, but the lifted gate must hold no
    // matter how the principal arrived.
    let provider = Arc::new(StaticStoreProvider::new(vec![]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        Some(member("carol")),
    );
    let result = handler.list_stores_inner(&no_extensions()).await;
    assert_ne!(result.is_error, Some(true));
}

// -----------------------------------------------------------------
// T5: D7 store filtering by principal
// -----------------------------------------------------------------

fn store_with_visibility(name: &str, visibility: &str) -> AvailableStore {
    AvailableStore::new(
        crate::tools::StoreDescriptor {
            id: format!("id-{name}"),
            name: name.to_string(),
            visibility: visibility.to_string(),
        },
        Box::new(localdb_core::store::FakeStore::new()),
    )
}

fn stores_json(result: &CallToolResult) -> Vec<String> {
    let text = result.content[0].as_text().unwrap().text.clone();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    parsed["stores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn admin_sees_every_store_private_and_shared() {
    let provider = Arc::new(StaticStoreProvider::new(vec![
        store_with_visibility("shared-docs", "shared"),
        store_with_visibility("secret-docs", "private"),
    ]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let result = handler
        .list_stores_inner(&http_extensions_with_principal(Principal::local_trust()))
        .await;
    assert_ne!(result.is_error, Some(true));
    let mut names = stores_json(&result);
    names.sort();
    assert_eq!(names, vec!["secret-docs", "shared-docs"]);
}

#[tokio::test]
async fn member_sees_only_granted_shared_stores() {
    let provider = Arc::new(StaticStoreProvider::new(vec![
        store_with_visibility("granted-shared", "shared"),
        store_with_visibility("ungranted-shared", "shared"),
        store_with_visibility("secret-private", "private"),
    ]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let principal = Principal {
        user_id: "u1".into(),
        name: "member-with-grant".into(),
        role: Role::Member,
        access: StoreAccess::Granted(["granted-shared".to_string()].into_iter().collect()),
    };
    let result = handler
        .list_stores_inner(&http_extensions_with_principal(principal))
        .await;
    assert_ne!(result.is_error, Some(true));
    let names = stores_json(&result);
    assert_eq!(names, vec!["granted-shared"]);
}

#[tokio::test]
async fn member_search_naming_an_ungranted_store_is_forbidden_not_not_found() {
    // specs/05-surfaces.md §3.1: a store the member holds no grant for,
    // but that genuinely exists, must be `forbidden` (403 on the HTTP
    // surface) — not collapsed into `store_not_found`, which would be
    // inconsistent with `server::search_service::SearchService::query`'s
    // identical `store_filter` check.
    let provider = Arc::new(StaticStoreProvider::new(vec![store_with_visibility(
        "ungranted",
        "shared",
    )]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let args = SearchArgs {
        query: "hello".to_string(),
        stores: Some(vec!["ungranted".to_string()]),
        limit: None,
        content_length: None,
        filters: Default::default(),
    };
    let result = handler
        .search_inner(args, &http_extensions_with_principal(member("dana")))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(error_code(&result), "forbidden");
}

#[tokio::test]
async fn member_search_naming_a_truly_unknown_store_is_store_not_found() {
    // A name absent from the full store list entirely (not merely
    // filtered out by D7) must still be `store_not_found` — only
    // "exists but unreadable" is promoted to `forbidden`.
    let provider = Arc::new(StaticStoreProvider::new(vec![store_with_visibility(
        "granted-shared",
        "shared",
    )]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let principal = Principal {
        user_id: "u1".into(),
        name: "member-with-grant".into(),
        role: Role::Member,
        access: StoreAccess::Granted(["granted-shared".to_string()].into_iter().collect()),
    };
    let args = SearchArgs {
        query: "hello".to_string(),
        stores: Some(vec!["does-not-exist".to_string()]),
        limit: None,
        content_length: None,
        filters: Default::default(),
    };
    let result = handler
        .search_inner(args, &http_extensions_with_principal(principal))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(error_code(&result), "store_not_found");
}

#[tokio::test]
async fn admin_search_naming_any_store_works() {
    // An admin (or local_trust) is never subject to D7 filtering, so
    // naming any real store — private or shared — must succeed.
    let provider = Arc::new(StaticStoreProvider::new(vec![
        store_with_visibility("shared-docs", "shared"),
        store_with_visibility("secret-docs", "private"),
    ]));
    let handler = McpHandler::new(
        provider,
        Arc::new(crate::tools::StoresBackend::new(&[])),
        embedder(),
        false,
        None,
    );
    let args = SearchArgs {
        query: "hello".to_string(),
        stores: Some(vec!["secret-docs".to_string()]),
        limit: None,
        content_length: None,
        filters: Default::default(),
    };
    let result = handler
        .search_inner(
            args,
            &http_extensions_with_principal(Principal::local_trust()),
        )
        .await;
    assert_ne!(result.is_error, Some(true));
}

#[test]
fn named_store_authorization_preserves_id_precedence() {
    let mut allowed = store_with_visibility("shadow", "shared");
    allowed.descriptor.id = "allowed-id".into();
    let mut denied = store_with_visibility("secret", "shared");
    denied.descriptor.id = "shadow".into();
    let full = vec![allowed.clone(), denied];
    let error =
        McpHandler::forbidden_for_named_unreadable_store(&full, &[allowed], &["shadow".into()])
            .unwrap();
    assert_eq!(error_code(&error), "forbidden");
}
