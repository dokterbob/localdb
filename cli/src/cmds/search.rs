use localdb_core::citation::Citation;
use localdb_core::{config::loader::ConfigLoader, Error, SearchFilters};
use serde_json::json;
use server::search_service::SearchRequest;

use crate::{
    app_db::{load_config_lenient, open_app_db_lenient_or_exit},
    app_db::{resolve_store_scope_inner, AppDb, StoreScopePolicy},
    command_table::{dispatch, DaemonAwareCommand},
    daemon_client::{daemon_request_async, CliContext},
    normalize::{exit_err, format_snippet, print_json, validate_store_name},
};

/// `localdb search <query> [--limit N] [--content-length N] [filters...]`
pub fn run_search(
    ctx: &CliContext,
    query: &str,
    limit: usize,
    content_length: usize,
    filters: SearchFilters,
) {
    // F9: Reject --limit 0.
    if limit == 0 {
        exit_err(
            &Error::InvalidRequest {
                message: "--limit must be at least 1".to_string(),
            },
            ctx.json,
        );
    }

    // A9-safety: validate --store name if given.
    for store_name in &ctx.stores {
        if let Err(e) = validate_store_name(store_name) {
            exit_err(&e, ctx.json);
        }
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(run_search_async(ctx, query, limit, content_length, filters));
}

/// `search`'s table entry (issue #187 stage 5). `Outcome` is `Vec<Citation>`
/// in both modes: the daemon branch used to hand-walk the raw JSON response
/// and silently drop `heading_path` (issue #187 §2) because it rendered
/// straight from `serde_json::Value` instead of deserializing into the same
/// `Citation` type embedded mode already produced. Deserializing
/// `value["citations"]` here, once, means there is exactly one citation
/// renderer (`citation_headline`) and it is structurally impossible for the
/// daemon path to drop a field the embedded path prints.
pub(crate) struct SearchCmd<'a> {
    pub(crate) query: &'a str,
    pub(crate) limit: usize,
    pub(crate) filters: SearchFilters,
}

/// Fail unless the daemon at `base_url` advertises search-filter support.
///
/// Absence is treated as unsupported, which is the only safe reading: a
/// daemon older than the `features` field omits it entirely, and one older
/// than search filters would silently drop them and answer unfiltered.
/// Exits 5 (unavailable) — the daemon is running and healthy, it just cannot
/// do what was asked — and names the fix, since restarting it resolves this
/// permanently.
async fn require_daemon_search_filter_support(base_url: &str) -> Result<(), Error> {
    let url = format!("{base_url}/v1/status");
    let status = daemon_request_async(reqwest::Method::GET, &url, None).await?;
    let supported = status
        .get("features")
        .and_then(|f| f.as_array())
        .is_some_and(|features| {
            features
                .iter()
                .any(|f| f.as_str() == Some("search_filters"))
        });

    if supported {
        return Ok(());
    }
    Err(Error::DaemonCapabilityUnavailable {
        message: "the running daemon predates search filters and would ignore them, \
                  returning unfiltered results; restart it (`localdb serve`) to use \
                  --path/--mime/date filters, or stop it to search in embedded mode"
            .to_string(),
    })
}

impl DaemonAwareCommand for SearchCmd<'_> {
    type Outcome = Vec<Citation>;

    // specs/05-surfaces.md §2.2: the one deliberate zero-store exit-0
    // exception (`AllStoresAllowEmpty`) — a fresh, storeless database has no
    // results, not an error (test `cli_integration.rs` ~2476).
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStoresAllowEmpty;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        // A daemon predating search filters ignores the new request fields
        // rather than rejecting them — `SearchRequest` has no
        // `deny_unknown_fields` — and answers as though no filter had been
        // asked for. That is the worst possible failure for a scoping
        // request: the caller gets a full, unfiltered result set that looks
        // like a correctly narrowed one. A long-lived daemon outliving a
        // binary upgrade makes this reachable in normal use.
        //
        // So when filters are actually set, confirm the daemon advertises
        // support before sending them. Only paid for when filtering; an
        // unfiltered search still goes straight to the POST below.
        if self.filters.is_any_set() {
            require_daemon_search_filter_support(base_url).await?;
        }

        let url = format!("{base_url}/v1/search");
        // Serialize the shared `SearchRequest` struct rather than hand-building
        // a `serde_json::json!` body: field names between this
        // CLI-daemon path and `POST /v1/search`'s own `Deserialize` impl can
        // then never drift apart. Filter values are sent raw (unparsed) —
        // the daemon runs the exact same `SearchFilters::into_metadata_filters`
        // validation embedded mode runs, so a malformed date bound surfaces
        // as the same `invalid_request` / exit 2 either way.
        let request = SearchRequest {
            query: self.query.to_string(),
            store_filter: ctx.stores.clone(),
            limit: self.limit,
            cursor: None,
            filters: self.filters.clone(),
        };
        let body = serde_json::to_value(&request).map_err(|e| Error::Internal {
            message: format!("cannot serialize search request: {e}"),
            correlation_id: "daemon_search_request_shape".to_string(),
        })?;
        let value = daemon_request_async(reqwest::Method::POST, &url, Some(body)).await?;
        let citations_json = value.get("citations").cloned().unwrap_or(json!([]));
        serde_json::from_value(citations_json).map_err(|e| Error::Internal {
            message: format!("cannot parse daemon search response citations: {e}"),
            correlation_id: "daemon_search_citations_shape".to_string(),
        })
    }

    async fn run_embedded(
        &self,
        ctx: &CliContext,
        config_loader: &ConfigLoader,
        db: &AppDb,
    ) -> Result<Self::Outcome, Error> {
        use localdb_core::clamp_search_limit;
        use localdb_core::search::{QueryRequest, SearchOrchestrator, StoreHandle};

        // Validate the filters first, before the empty-store return and
        // before the embedder is built. Argument validity is a property of
        // the invocation, not of database or daemon state, so a malformed
        // `--added-after not-a-date` must exit 2 identically whether the
        // database is empty, populated, or fronted by a daemon. Doing it
        // later meant a storeless database reported "no results" and exit 0,
        // and a populated one could fail on provider configuration — or
        // trigger a model download — before ever mentioning the bad value.
        let filters = self.filters.clone().into_metadata_filters()?;

        // specs/05-surfaces.md §2.2, via the one shared resolver every other
        // `-s`-accepting command uses. `AllStoresAllowEmpty` is what makes a
        // fresh, storeless database return no results and exit 0 rather than
        // exit 2.
        let rows = resolve_store_scope_inner(ctx, db, Self::SCOPE_POLICY).await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let mut store_handles = Vec::with_capacity(rows.len());
        for store_row in &rows {
            let handle = db.backend().retrieval_store(&store_row.id).await?;
            store_handles.push(StoreHandle {
                id: store_row.id.clone(),
                name: store_row.name.clone(),
                store: handle,
            });
        }

        let embed_policy = &config_loader.config.defaults.indexing.embedding;
        let models_dir = config_loader.paths.models_dir.clone();
        let embedder = embed::create_embedder(
            embed_policy,
            &config_loader.config.providers,
            Some(&models_dir),
            &(&config_loader.config.http).into(),
        )
        .map_err(Error::from)?;
        // Parity with the daemon path (issue #187 review, finding 1):
        // `POST /v1/search` clamps `limit` to `SEARCH_MAX_LIMIT` before it
        // ever reaches `SearchOrchestrator::query`
        // (`server::search_service::clamp_search_limit`), and so does the
        // MCP `search` tool (`mcp::tools::resolve_search_limit`). Without an
        // equivalent clamp here, `localdb search foo --limit 5000` returned
        // a different result count depending on whether a daemon happened
        // to be running — the exact asymmetry this issue is about fixing.
        let request = QueryRequest {
            query: self.query.to_string(),
            leg_k: None,
            top_n: Some(clamp_search_limit(self.limit)),
            filters,
        };

        SearchOrchestrator::query(&store_handles, embedder.as_ref(), &request)
            .await
            .map(|response| response.citations)
    }
}

/// The one-line citation headline for human output: `uri`, then the heading
/// path (if any), then the page number `(p.N)` for paginated sources (#103).
fn citation_headline(citation: &Citation) -> String {
    let heading = if citation.heading_path.is_empty() {
        String::new()
    } else {
        format!(" > {}", citation.heading_path.join(" > "))
    };
    let page = citation
        .block
        .page
        .map(|p| format!(" (p.{p})"))
        .unwrap_or_default();
    format!("{}{}{}", citation.uri, heading, page)
}

/// The one renderer for `search`'s `Outcome`, consumed identically whether
/// `citations` came from the embedded query path or a deserialized daemon
/// response.
fn render_search_output(
    citations: &[Citation],
    query: &str,
    content_length: usize,
    json_mode: bool,
) {
    if json_mode {
        let json_citations: Vec<serde_json::Value> = citations
            .iter()
            .map(|c| serde_json::to_value(c).unwrap_or(json!({})))
            .collect();
        print_json(&json!({ "citations": json_citations }));
    } else if citations.is_empty() {
        println!("No results for '{}'.", query);
    } else {
        for (i, citation) in citations.iter().enumerate() {
            println!("{}. {}", i + 1, citation_headline(citation));
            println!("   {}", format_snippet(&citation.snippet, content_length));
            println!();
        }
    }
}

pub(crate) async fn run_search_async(
    ctx: &CliContext,
    query: &str,
    limit: usize,
    content_length: usize,
    filters: SearchFilters,
) {
    // F1-cli: use lenient loader so search works even with malformed config.
    let config_loader = load_config_lenient(ctx).await;
    let citations = dispatch(
        &SearchCmd {
            query,
            limit,
            filters,
        },
        ctx,
        &config_loader,
        || open_app_db_lenient_or_exit(ctx, &config_loader),
    )
    .await;
    render_search_output(&citations, query, content_length, ctx.json);
}

#[cfg(test)]
mod tests {
    use super::citation_headline;
    use localdb_core::citation::{
        ChunkPosition, Citation, CitationBlock, CitationLocation, CitationProvenance,
        CitationStore, Score,
    };
    use localdb_core::types::Span;

    fn citation_with(page: Option<u32>, heading: Vec<String>) -> Citation {
        Citation {
            chunk_id: "chunk".to_string(),
            resource_id: "res".to_string(),
            store: CitationStore {
                id: "01HN1Y28MYWN6X5DSKZMNE1T5W".to_string(),
                name: "s".to_string(),
            },
            uri: "file:///docs/paper.pdf".to_string(),
            title: None,
            heading_path: heading,
            block: CitationBlock {
                seq: 0,
                kind: Some("text".to_string()),
                page,
            },
            chunk_position: ChunkPosition { seq_in_block: 0 },
            location: CitationLocation {
                span: Span::new(0, 4),
                window_block_seqs: vec![],
            },
            snippet: "text".to_string(),
            score: Score {
                fused: 1.0,
                dense: None,
                bm25: None,
            },
            provenance: CitationProvenance {
                fetched_at: "2026-06-10T12:00:00Z".to_string(),
                content_hash: "abc".to_string(),
            },
            metadata: Default::default(),
        }
    }

    #[test]
    fn headline_appends_page_when_present() {
        let line = citation_headline(&citation_with(Some(12), vec![]));
        assert_eq!(line, "file:///docs/paper.pdf (p.12)");
    }

    #[test]
    fn headline_omits_page_when_absent() {
        let line = citation_headline(&citation_with(None, vec![]));
        assert_eq!(line, "file:///docs/paper.pdf");
    }

    #[test]
    fn headline_combines_heading_path_and_page() {
        let line = citation_headline(&citation_with(
            Some(3),
            vec!["Intro".to_string(), "Setup".to_string()],
        ));
        assert_eq!(line, "file:///docs/paper.pdf > Intro > Setup (p.3)");
    }
}
