use std::path::Path;

use localdb_core::{config::loader::ConfigLoader, Error};
use serde_json::json;

use crate::{
    app_db::load_app_db_lenient,
    daemon_client::{daemon_request_async, probe_daemon, CliContext, DaemonState},
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

pub(crate) enum SearchMode {
    Daemon { base_url: String },
    Embedded,
}

pub(crate) enum SearchOutput {
    Daemon {
        value: serde_json::Value,
        query: String,
    },
    Embedded {
        citations: Vec<localdb_core::citation::Citation>,
        query: String,
    },
}

pub(crate) async fn resolve_search_targets(
    ctx: &CliContext,
    mode: &SearchMode,
) -> Result<Vec<(localdb_core::search::StoreHandle, String)>, Error> {
    match mode {
        SearchMode::Daemon { .. } => Ok(Vec::new()),
        SearchMode::Embedded => {
            let (_config_loader, db) = load_app_db_lenient(ctx).await;
            let runtime_stores = db.backend().list_stores().await?;

            if !ctx.stores.is_empty() {
                let runtime_names: std::collections::HashSet<&str> =
                    runtime_stores.iter().map(|s| s.name.as_str()).collect();
                for name in &ctx.stores {
                    if !runtime_names.contains(name.as_str()) {
                        return Err(Error::StoreNotFound { id: name.clone() });
                    }
                }
            }

            let store_names: Vec<String> = if ctx.stores.is_empty() {
                runtime_stores.iter().map(|s| s.name.clone()).collect()
            } else {
                ctx.stores.clone()
            };

            let mut store_handles = Vec::new();
            for name in &store_names {
                if let Some(store_row) = runtime_stores.iter().find(|s| s.name == *name) {
                    let handle = db.backend().retrieval_store(&store_row.id).await?;
                    let store_name = store_row.name.clone();
                    store_handles.push((
                        localdb_core::search::StoreHandle {
                            id: store_row.id.clone(),
                            name: store_name.clone(),
                            store: handle,
                        },
                        store_name,
                    ));
                }
            }

            Ok(store_handles)
        }
    }
}

pub(crate) fn print_search_output(out: SearchOutput, content_length: usize, json_mode: bool) {
    match out {
        SearchOutput::Daemon { value, query } => {
            if json_mode {
                print_json(&value);
            } else {
                let empty = vec![];
                let citations = value
                    .get("citations")
                    .and_then(|c| c.as_array())
                    .unwrap_or(&empty);
                if citations.is_empty() {
                    println!("No results for '{}'.", query);
                } else {
                    for (i, cit) in citations.iter().enumerate() {
                        let uri = cit.get("uri").and_then(|u| u.as_str()).unwrap_or("?");
                        let snippet = cit.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
                        let page = cit
                            .get("block")
                            .and_then(|b| b.get("page"))
                            .and_then(|p| p.as_u64())
                            .map(|p| format!(" (p.{p})"))
                            .unwrap_or_default();
                        println!("{}. {}{}", i + 1, uri, page);
                        println!("   {}", format_snippet(snippet, content_length));
                        println!();
                    }
                }
            }
        }
        SearchOutput::Embedded { citations, query } => {
            let json_citations: Vec<serde_json::Value> = citations
                .iter()
                .map(|c| serde_json::to_value(c).unwrap_or(json!({})))
                .collect();

            if json_mode {
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
    }
}

/// The one-line citation headline for human output: `uri`, then the heading
/// path (if any), then the page number `(p.N)` for paginated sources (#103).
fn citation_headline(citation: &localdb_core::citation::Citation) -> String {
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

fn detect_search_mode(data_dir: &Path, daemon_url: Option<&str>) -> SearchMode {
    match probe_daemon(data_dir, daemon_url) {
        DaemonState::Running { base_url } => SearchMode::Daemon { base_url },
        DaemonState::NotRunning => SearchMode::Embedded,
    }
}

async fn request_daemon_search(
    ctx: &CliContext,
    base_url: &str,
    query: &str,
    limit: usize,
) -> Result<serde_json::Value, Error> {
    let url = format!("{base_url}/v1/search");
    let mut body = json!({
        "query": query,
        "limit": limit,
    });
    if !ctx.stores.is_empty() {
        body["store_filter"] = serde_json::Value::Array(
            ctx.stores
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
    }
    daemon_request_async(reqwest::Method::POST, &url, Some(body)).await
}

async fn query_embedded_search(
    config_loader: &ConfigLoader,
    targets: Vec<(localdb_core::search::StoreHandle, String)>,
    query: &str,
    limit: usize,
) -> Result<Vec<localdb_core::citation::Citation>, Error> {
    use localdb_core::search::{QueryRequest, SearchOrchestrator};

    let embed_policy = &config_loader.config.defaults.indexing.embedding;
    let models_dir = config_loader.paths.models_dir.clone();
    let embedder = embed::create_embedder(
        embed_policy,
        &config_loader.config.providers,
        Some(&models_dir),
    )
    .map_err(Error::from)?;
    let store_handles: Vec<_> = targets.into_iter().map(|(handle, _name)| handle).collect();
    let request = QueryRequest {
        query: query.to_string(),
        leg_k: None,
        top_n: Some(limit),
        filters: vec![],
    };

    SearchOrchestrator::query(&store_handles, embedder.as_ref(), &request)
        .await
        .map(|response| response.citations)
}

async fn execute_search_mode(
    ctx: &CliContext,
    config_loader: &ConfigLoader,
    mode: SearchMode,
    targets: Vec<(localdb_core::search::StoreHandle, String)>,
    query: &str,
    limit: usize,
) -> Result<SearchOutput, Error> {
    match mode {
        SearchMode::Daemon { base_url } => {
            let value = request_daemon_search(ctx, &base_url, query, limit).await?;
            Ok(SearchOutput::Daemon {
                value,
                query: query.to_string(),
            })
        }
        SearchMode::Embedded if targets.is_empty() => Ok(SearchOutput::Embedded {
            citations: Vec::new(),
            query: query.to_string(),
        }),
        SearchMode::Embedded => {
            let citations = query_embedded_search(config_loader, targets, query, limit).await?;
            Ok(SearchOutput::Embedded {
                citations,
                query: query.to_string(),
            })
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
    let (config_loader, _db) = load_app_db_lenient(ctx).await;
    let mode = detect_search_mode(&config_loader.paths.data_dir, ctx.daemon_url.as_deref());
    let targets = match resolve_search_targets(ctx, &mode).await {
        Ok(targets) => targets,
        Err(e) => exit_err(&e, ctx.json),
    };

    match execute_search_mode(ctx, &config_loader, mode, targets, query, limit).await {
        Ok(output) => print_search_output(output, content_length, ctx.json),
        Err(e) => exit_err(&e, ctx.json),
    }
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
