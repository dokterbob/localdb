use localdb_core::citation::Citation;
use localdb_core::{config::loader::ConfigLoader, Error};
use serde_json::json;

use crate::{
    app_db::{load_config_lenient, open_app_db_lenient_or_exit},
    app_db::{resolve_store_scope_inner, AppDb, StoreScopePolicy},
    command_table::{dispatch, DaemonAwareCommand},
    daemon_client::{daemon_request_async, CliContext},
    normalize::{exit_err, format_snippet, print_json, validate_store_name},
};

/// `localdb search <query> [--limit N] [--content-length N]`
pub fn run_search(ctx: &CliContext, query: &str, limit: usize, content_length: usize) {
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
    rt.block_on(run_search_async(ctx, query, limit, content_length));
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
}

impl DaemonAwareCommand for SearchCmd<'_> {
    type Outcome = Vec<Citation>;

    // specs/05-surfaces.md §2.2: the one deliberate zero-store exit-0
    // exception (`AllStoresAllowEmpty`) — a fresh, storeless database has no
    // results, not an error (test `cli_integration.rs` ~2476).
    const SCOPE_POLICY: StoreScopePolicy = StoreScopePolicy::AllStoresAllowEmpty;

    async fn run_daemon(&self, ctx: &CliContext, base_url: &str) -> Result<Self::Outcome, Error> {
        let url = format!("{base_url}/v1/search");
        let mut body = json!({
            "query": self.query,
            "limit": self.limit,
        });
        if !ctx.stores.is_empty() {
            body["store_filter"] = serde_json::Value::Array(
                ctx.stores
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            );
        }
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
            filters: vec![],
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
) {
    // F1-cli: use lenient loader so search works even with malformed config.
    let config_loader = load_config_lenient(ctx).await;
    let citations = dispatch(&SearchCmd { query, limit }, ctx, &config_loader, || {
        open_app_db_lenient_or_exit(ctx, &config_loader)
    })
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
