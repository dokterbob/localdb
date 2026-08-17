//! `ProxyHandler` — forwards `tools/list`/`tools/call` to a running daemon's
//! `/mcp` HTTP route (Phase 3 scope, see `lib.rs`'s crate-level doc comment),
//! optionally narrowed to a `--store` scope (specs/05-surfaces.md §4.2.1).
//!
//! Every other `ServerHandler` in this crate (`handler::McpHandler`) is
//! macro-native: `#[tool_router]`/`#[tool_handler]` generates dispatch from
//! typed argument structs it owns ahead of time. `ProxyHandler` cannot be
//! macro-native — it has no argument structs of its own and does not know
//! the upstream's tool set ahead of time (that set is whatever store
//! snapshot the daemon happened to build at its own startup, see
//! `server::mcp_bridge::build_available_stores`'s doc comment) — so it just
//! relays whatever request arrives to the upstream connection. This is the
//! one hand-written `ServerHandler` impl in the migration, deliberately.
//!
//! ## Why scope is enforced on tool *arguments*
//!
//! Unscoped (no `--store`), this really is a verbatim relay and nothing here
//! inspects a request. Scoped, it has to — and the tool arguments are the
//! only channel available. rmcp's `StreamableHttpService` (`http.rs`) takes a
//! synchronous `Fn() -> Result<S, io::Error>` service factory with no access
//! to the HTTP request, so the daemon cannot hand out a per-connection scoped
//! handler however the client asks: not via `/mcp?store=x`, not via a header.
//! But `search.stores`, `get_document.store`, `get_chunks.store`, and
//! `list_documents.store` exist precisely to name stores, and `tools.rs`
//! already honours them — so the scope travels as an argument, per request,
//! instead of per connection.
//!
//! **This is scoping, not a security boundary.** The daemon's `/mcp` is
//! loopback and unauthenticated: anything that can open a socket can bypass
//! `localdb mcp` and talk to the unscoped endpoint directly. It stops an
//! agent from *accidentally* reading another project's docs; it does not
//! contain a hostile one. See specs/05-surfaces.md §4.2.1.

use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleClient, RoleServer, RunningService, ServiceError},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ErrorData as McpError, ServerHandler, ServiceExt,
};

/// One store the upstream daemon exposes over MCP, as reported by its own
/// `list_stores` tool.
///
/// Both fields matter for scope enforcement: `tools::select_mcp_stores`
/// resolves a caller-supplied store argument by **id first, then name**, so
/// deciding whether a value is in scope means reproducing that exact rule
/// against the upstream's full store set — see `ProxyScope::resolve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamStore {
    pub id: String,
    pub name: String,
}

/// A resolved `--store` scope: every store the upstream has, plus which of
/// them the caller is allowed to reach.
///
/// The full set is kept, not just the allowed subset, because a value that
/// matches an allowed store's *name* may simultaneously be a *different*,
/// out-of-scope store's *id* — and the upstream would resolve the id. Judging
/// membership against the allowed subset alone would green-light that request
/// and leak the out-of-scope store. See `resolve`.
#[derive(Debug, Clone)]
pub struct ProxyScope {
    upstream_stores: Vec<UpstreamStore>,
    allowed_ids: Vec<String>,
}

impl ProxyScope {
    /// Resolve a caller-supplied store argument (an id or a name) to the
    /// store the *upstream* would resolve it to, or `None` if it names no
    /// store at all.
    ///
    /// Id pass before name pass, matching `tools::select_mcp_stores` exactly:
    /// ids are unique and machine-generated, names are user-chosen and may
    /// legitimately collide with some other store's id.
    fn resolve(&self, value: &str) -> Option<&UpstreamStore> {
        self.upstream_stores
            .iter()
            .find(|s| s.id == value)
            .or_else(|| self.upstream_stores.iter().find(|s| s.name == value))
    }

    /// Whether `id` is inside the scope.
    fn allows(&self, id: &str) -> bool {
        self.allowed_ids.iter().any(|a| a == id)
    }

    /// Human-readable list of the store *names* in scope, for error messages
    /// — the caller passed names on the command line, so names are what they
    /// recognize, even though the wire now carries ids.
    fn allowed_names(&self) -> String {
        let names: Vec<&str> = self
            .upstream_stores
            .iter()
            .filter(|s| self.allows(&s.id))
            .map(|s| s.name.as_str())
            .collect();
        names.join(", ")
    }

    /// Resolve one caller-supplied store value to a canonical, in-scope store
    /// **id**, or produce the tool-level rejection.
    ///
    /// Returning the id (rather than passing the caller's spelling through)
    /// is what closes the shadowing hole: whatever the caller wrote, the
    /// upstream now receives an exact id, and its id-first rule guarantees
    /// that resolves to the store this function actually approved.
    // `CallToolResult` crossed clippy's result_large_err threshold once the
    // workspace `schemars` dep gained `preserve_order` (serde_json's Map
    // switches from BTreeMap to IndexMap, growing `serde_json::Value`).
    // Boxing every `Err(CallToolResult)` call site in this crate is out of
    // scope for that change; allow the lint here instead.
    #[allow(clippy::result_large_err)]
    fn canonicalize(&self, value: &str) -> Result<String, CallToolResult> {
        match self.resolve(value) {
            Some(store) if self.allows(&store.id) => Ok(store.id.clone()),
            _ => Err(scope_rejection(value, &self.allowed_names())),
        }
    }
}

/// Tool-level `invalid_request` for a store argument outside the scope.
///
/// Tool-level, not protocol-level: this is the same tier the upstream's own
/// store validation uses (`tools::typed_error`), and it carries the same
/// `{"error": {"code", "message"}}` body, so a client cannot tell a scope
/// rejection apart from any other argument rejection by shape alone
/// (specs/05-surfaces.md §4.3).
fn scope_rejection(requested: &str, allowed: &str) -> CallToolResult {
    let v = serde_json::json!({
        "error": {
            "code": "invalid_request",
            "message": format!(
                "store '{requested}' is outside this session's --store scope; allowed: [{allowed}]"
            ),
        }
    });
    CallToolResult::error(vec![Content::text(
        serde_json::to_string_pretty(&v).unwrap_or_default(),
    )])
}

/// Why `ProxyHandler::connect` failed, split so `cli` can map each to the
/// stable exit code it already uses for that class of failure.
///
/// Without this split every connect failure would collapse into
/// `daemon_unreachable`/exit 5, and an unknown `--store` name would report
/// "daemon is unreachable" — when the daemon answered perfectly well and it
/// was the *name* that was wrong. Embedded mode exits 3 for that; proxied
/// mode must match.
#[derive(Debug)]
pub enum ProxyConnectError {
    /// The upstream could not be reached, or its handshake/`list_stores`
    /// call failed -> `daemon_unreachable`, exit 5.
    Unreachable(anyhow::Error),
    /// A `--store` name the daemon does not have -> `store_not_found`, exit 3.
    StoreNotFound(String),
}

impl std::fmt::Display for ProxyConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => write!(f, "mcp proxy: upstream unreachable: {e}"),
            Self::StoreNotFound(name) => write!(f, "store not found: {name}"),
        }
    }
}

impl std::error::Error for ProxyConnectError {}

/// A `ServerHandler` that proxies every `tools/list`/`tools/call` request to
/// an upstream rmcp server reached over Streamable HTTP — used when
/// `localdb mcp` runs while a daemon is already up (see
/// `entrypoint::serve_proxied_stdio`).
///
/// Holds the upstream MCP client session for the handler's whole lifetime:
/// `RunningService` owns the background task pumping the HTTP transport, so
/// keeping it as a field (rather than just a `Peer`) is what keeps that task
/// — and the upstream `initialize` handshake it already completed — alive
/// for as long as this stdio process serves requests.
pub struct ProxyHandler {
    upstream: RunningService<RoleClient, rmcp::model::ClientInfo>,
    /// `None` = unscoped (no `--store` given): every request relays verbatim.
    scope: Option<ProxyScope>,
}

impl ProxyHandler {
    /// Connect to `{daemon_base_url}/mcp`, complete the upstream MCP
    /// `initialize` handshake, and resolve `scope_names` against the store
    /// set that upstream actually exposes.
    ///
    /// `scope_names` is the CLI's `--store` list; empty means unscoped.
    /// Names are matched against store **names** only — `-s` is documented as
    /// taking a name (specs/05-surfaces.md §2.2), and this keeps proxied mode
    /// resolving `-s` exactly as embedded mode's `get_store_by_name` does.
    /// (The looser id-or-name matching applies only to *tool* arguments,
    /// where #144's citation round-trip needs it.)
    ///
    /// The scope is resolved against the upstream's own `list_stores` — not
    /// `GET /v1/stores` — because the two can genuinely disagree: `/mcp`
    /// serves a startup-time snapshot (see `http.rs`), so a store added since
    /// then is in `/v1/stores` but not reachable over MCP. Validating against
    /// the set the tools will actually see is what makes an accepted `-s`
    /// name mean something.
    ///
    /// # Errors
    /// [`ProxyConnectError::Unreachable`] if the HTTP transport cannot be
    /// constructed, the upstream handshake fails (e.g. the daemon went down
    /// between `probe_daemon` succeeding in `cli` and this call), or its
    /// `list_stores` answer cannot be parsed;
    /// [`ProxyConnectError::StoreNotFound`] if a requested name is absent
    /// from the upstream's store set.
    pub async fn connect(
        daemon_base_url: &str,
        scope_names: &[String],
    ) -> Result<Self, ProxyConnectError> {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("{daemon_base_url}/mcp")),
        );
        let upstream = rmcp::model::ClientInfo::default()
            .serve(transport)
            .await
            .map_err(|e| ProxyConnectError::Unreachable(e.into()))?;

        if scope_names.is_empty() {
            return Ok(Self {
                upstream,
                scope: None,
            });
        }

        let upstream_stores = fetch_upstream_stores(&upstream)
            .await
            .map_err(ProxyConnectError::Unreachable)?;

        let mut allowed_ids: Vec<String> = Vec::new();
        for name in scope_names {
            match upstream_stores.iter().find(|s| &s.name == name) {
                Some(store) => {
                    // Dedupe: `-s a -s a` is one store in scope, matching
                    // `resolve_store_scope_inner`'s explicit-name path.
                    if !allowed_ids.contains(&store.id) {
                        allowed_ids.push(store.id.clone());
                    }
                }
                None => return Err(ProxyConnectError::StoreNotFound(name.clone())),
            }
        }

        Ok(Self {
            upstream,
            scope: Some(ProxyScope {
                upstream_stores,
                allowed_ids,
            }),
        })
    }

    /// Relay a `tools/call` to the upstream unchanged.
    async fn relay(&self, request: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        self.upstream
            .call_tool(request)
            .await
            .map_err(upstream_error_to_mcp)
    }

    /// `search`: intersect the caller's `stores` with the scope, or inject
    /// the whole scope when they named none.
    async fn call_search_scoped(
        &self,
        mut request: CallToolRequestParams,
        scope: &ProxyScope,
    ) -> Result<CallToolResult, McpError> {
        let args = request.arguments.get_or_insert_with(Default::default);

        // `null` is treated as absent, matching `#[serde(default)]` on
        // `SearchArgs::stores`. An explicitly empty array is *also* absent as
        // far as `select_mcp_stores` is concerned (it means "all stores"), so
        // it must be filled in too rather than passed through to mean "every
        // store the daemon has".
        let requested: Option<Vec<String>> = match args.get("stores") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Array(items)) if items.is_empty() => None,
            Some(serde_json::Value::Array(items)) => {
                let mut names = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(s) => names.push(s.to_string()),
                        // A non-string entry can't name a store; let the
                        // upstream's own deserializer author that error
                        // rather than inventing a second dialect for it.
                        None => return self.relay(request).await,
                    }
                }
                Some(names)
            }
            // Wrong type entirely — same reasoning as above.
            Some(_) => return self.relay(request).await,
        };

        let effective: Vec<String> = match requested {
            None => scope.allowed_ids.clone(),
            Some(names) => {
                let mut ids = Vec::with_capacity(names.len());
                for name in &names {
                    match scope.canonicalize(name) {
                        Ok(id) => ids.push(id),
                        Err(rejection) => return Ok(rejection),
                    }
                }
                ids
            }
        };

        args.insert("stores".to_string(), serde_json::json!(effective));
        self.relay(request).await
    }

    /// `get_document` / `get_chunks` / `list_documents`: each takes a single
    /// `store` argument (optional on the first two, required on
    /// `list_documents` — the injection/rejection logic below treats them
    /// identically either way, since it only ever inspects the raw JSON
    /// argument, not the tool's own required-ness).
    ///
    /// An explicit value is canonicalized and scope-checked. Rejecting an
    /// out-of-scope explicit value is the load-bearing half: injecting only
    /// when absent would let a caller name any store on the daemon and read
    /// it, which is the exact leak this scoping exists to prevent.
    async fn call_single_store_scoped(
        &self,
        mut request: CallToolRequestParams,
        scope: &ProxyScope,
    ) -> Result<CallToolResult, McpError> {
        let args = request.arguments.get_or_insert_with(Default::default);

        match args.get("store") {
            Some(serde_json::Value::String(value)) => {
                let id = match scope.canonicalize(value) {
                    Ok(id) => id,
                    Err(rejection) => return Ok(rejection),
                };
                args.insert("store".to_string(), serde_json::json!(id));
                self.relay(request).await
            }
            // Present but not a string: let the upstream's deserializer
            // author that error, as in `call_search_scoped`.
            Some(v) if !v.is_null() => self.relay(request).await,
            // Absent. These tools take one store, so a multi-store scope
            // can't be expressed in a single call — try each scoped store in
            // order and keep the first hit. That preserves the documented
            // "omitted store scans every available store, first match wins"
            // behavior (specs/05-surfaces.md §4), narrowed to the scope,
            // instead of degrading it into "you must now always pass store".
            _ => {
                let mut last: Option<CallToolResult> = None;
                for id in &scope.allowed_ids {
                    let mut attempt = request.clone();
                    attempt
                        .arguments
                        .get_or_insert_with(Default::default)
                        .insert("store".to_string(), serde_json::json!(id));
                    let result = self.relay(attempt).await?;
                    if result.is_error != Some(true) {
                        return Ok(result);
                    }
                    last = Some(result);
                }
                // Every scoped store said no — return the last store's own
                // error (a genuine `resource_not_found` from the upstream)
                // rather than inventing one. An empty scope has no upstream
                // answer at all, so it gets a scope rejection.
                Ok(last.unwrap_or_else(|| scope_rejection("<none>", &scope.allowed_names())))
            }
        }
    }

    /// `list_stores`: relay, then drop out-of-scope stores from the answer.
    ///
    /// Filtering the *response* (rather than refusing the call) keeps the
    /// tool useful — an agent still discovers what it may read — while making
    /// stores it may not read unenumerable, so it cannot even learn their
    /// names to try them in `search`.
    async fn call_list_stores_scoped(
        &self,
        request: CallToolRequestParams,
        scope: &ProxyScope,
    ) -> Result<CallToolResult, McpError> {
        let result = self.relay(request).await?;
        if result.is_error == Some(true) {
            return Ok(result);
        }

        let Some(mut parsed) = parse_result_json(&result) else {
            // Unparseable payload: the upstream answered in a shape this
            // build doesn't recognize. Withhold it rather than pass an
            // unfiltered store list through — failing closed is the only
            // safe direction for an access-scoping filter.
            return Ok(scope_rejection(
                "<unfilterable list_stores response>",
                &scope.allowed_names(),
            ));
        };

        let Some(stores) = parsed.get_mut("stores").and_then(|s| s.as_array_mut()) else {
            return Ok(scope_rejection(
                "<unfilterable list_stores response>",
                &scope.allowed_names(),
            ));
        };
        stores.retain(|s| {
            s.get("id")
                .and_then(|i| i.as_str())
                .is_some_and(|id| scope.allows(id))
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&parsed).unwrap_or_default(),
        )]))
    }
}

/// Ask the upstream for its store set via its own `list_stores` tool.
async fn fetch_upstream_stores(
    upstream: &RunningService<RoleClient, rmcp::model::ClientInfo>,
) -> anyhow::Result<Vec<UpstreamStore>> {
    let result = upstream
        .call_tool(CallToolRequestParams::new("list_stores"))
        .await?;
    if result.is_error == Some(true) {
        anyhow::bail!("upstream list_stores returned an error result");
    }
    let parsed = parse_result_json(&result)
        .ok_or_else(|| anyhow::anyhow!("upstream list_stores returned no parseable JSON"))?;
    let stores = parsed
        .get("stores")
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow::anyhow!("upstream list_stores response has no `stores` array"))?;

    let mut out = Vec::with_capacity(stores.len());
    for s in stores {
        let (Some(id), Some(name)) = (
            s.get("id").and_then(|v| v.as_str()),
            s.get("name").and_then(|v| v.as_str()),
        ) else {
            anyhow::bail!("upstream list_stores entry is missing `id` or `name`");
        };
        out.push(UpstreamStore {
            id: id.to_string(),
            name: name.to_string(),
        });
    }
    Ok(out)
}

/// Parse the JSON body every localdb MCP tool returns as its single text
/// content item (see `tools::success_json`).
fn parse_result_json(result: &CallToolResult) -> Option<serde_json::Value> {
    let text = &result.content.first()?.as_text()?.text;
    serde_json::from_str(text).ok()
}

/// Unwrap a `Peer<RoleClient>` call's `ServiceError` back into the tier the
/// upstream itself chose.
///
/// `ServiceError::McpError` is the upstream's own protocol-level `ErrorData`
/// — e.g. the "tool not found" error `handler::McpHandler`'s macro-generated
/// dispatch returns for an unregistered name (see `lib.rs`'s two-tier error
/// model doc) — and is forwarded unchanged so that tier survives the extra
/// hop. Any other `ServiceError` variant (transport closed, timeout, ...) is
/// a failure of the proxy hop itself, not a re-tiering of an upstream
/// answer: the upstream never got to answer at all, so there is no tier of
/// *its* to preserve. Those become a fresh protocol-level `internal_error`.
fn upstream_error_to_mcp(err: ServiceError) -> McpError {
    match err {
        ServiceError::McpError(e) => e,
        other => {
            McpError::internal_error(format!("mcp proxy: upstream request failed: {other}"), None)
        }
    }
}

impl ServerHandler for ProxyHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("localdb", env!("CARGO_PKG_VERSION")))
    }

    /// Relayed unchanged in both modes: the tool *set* is store-independent
    /// — the same five read-only tools regardless of which stores are in
    /// scope — so there is nothing here to filter.
    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.upstream
            .list_tools(request)
            .await
            .map_err(upstream_error_to_mcp)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let Some(scope) = &self.scope else {
            // Unscoped: byte-identical relay, including for unknown tool
            // names — the upstream owns "tool not found".
            return self.relay(request).await;
        };

        match request.name.as_ref() {
            "search" => self.call_search_scoped(request, scope).await,
            "get_document" | "get_chunks" | "list_documents" => {
                self.call_single_store_scoped(request, scope).await
            }
            "list_stores" => self.call_list_stores_scoped(request, scope).await,
            // Deliberately a denylist-free allowlist: a scoped session
            // relays only the five tools whose store semantics are known
            // here. Falling through to a verbatim relay would mean the first
            // mutating tool ever added (`--allow-write`, specs/05-surfaces.md
            // §4) silently bypasses the scope on the day it lands. Making
            // that a compile-time-visible gap is the point — the tripwire is
            // `mcp_tool_set_identical_with_and_without_allow_write`.
            other => Ok(CallToolResult::error(vec![Content::text(
                serde_json::to_string_pretty(&serde_json::json!({
                    "error": {
                        "code": "invalid_request",
                        "message": format!(
                            "tool '{other}' has no --store scoping rule, so it cannot be \
                             served on a store-scoped MCP session; re-run `localdb mcp` \
                             without --store to reach it"
                        ),
                    }
                }))
                .unwrap_or_default(),
            )])),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
