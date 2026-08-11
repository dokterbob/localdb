//! Ingestor-selection factory: `SourceSpec` kind -> concrete `Ingestor`.
//!
//! Extracted from the CLI's embedded composition loop
//! (`cli/src/cmds/index.rs`'s `run_embedded_index_with`, issue #187) so the
//! daemon's job-execution engine (`server::job_exec`) can build the exact
//! same ingestors the CLI does, instead of drifting from it. Both callers
//! are composition roots (specs/01-architecture.md §1): they wire
//! I/O-owning `ingest`/`fetch` types into the I/O-free `core` pipeline.

use localdb_core::parser::Parser;
use localdb_core::types::SourceSpec;
use localdb_core::ChainParser;
use localdb_core::Ingestor;

use crate::{FeedIngestor, FileIngestor, UrlIngestor};

/// Build the concrete `Ingestor` for `spec`'s kind.
///
/// `parser_chain` is consumed (parser instances aren't `Clone`, so callers
/// indexing multiple sources must build a fresh chain per call).
/// `url_fetcher` is the *unrestricted* client, used for the source's own
/// operator-configured URL (`url` sources, and a feed's own URL). `entry_fetcher`
/// must be a destination-restricted (`HttpUrlFetcher::new_public_only`)
/// client — it's used only for `Feed` sources' entry `<link>`s, which are
/// attacker-controlled content pulled from the feed document, not from the
/// operator. See `ingest::FeedIngestor::new`'s doc comment for the full
/// trust-boundary rationale. Both fetchers are cloned internally, so callers
/// pass shared references and keep ownership for reuse across sources.
pub fn build_ingestor_for_spec(
    spec: &SourceSpec,
    parser_chain: ChainParser,
    url_fetcher: &fetch::HttpUrlFetcher,
    entry_fetcher: &fetch::HttpUrlFetcher,
) -> Box<dyn Ingestor> {
    let parser: Box<dyn Parser> = Box::new(parser_chain);
    match spec {
        SourceSpec::Path { .. } => Box::new(FileIngestor::new(parser)),
        SourceSpec::Url { .. } => Box::new(UrlIngestor::new(parser, Box::new(url_fetcher.clone()))),
        SourceSpec::Feed { .. } => Box::new(FeedIngestor::new(
            parser,
            Box::new(url_fetcher.clone()),
            Box::new(entry_fetcher.clone()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::block::IngestorKind;

    fn parser_chain() -> ChainParser {
        extract::build_chain(&[
            "pdf".to_string(),
            "epub".to_string(),
            "office".to_string(),
            "html".to_string(),
            "markdown".to_string(),
            "plaintext".to_string(),
        ])
        .unwrap()
    }

    #[test]
    fn path_spec_builds_file_ingestor() {
        let fetcher = fetch::HttpUrlFetcher::new().unwrap();
        let ingestor = build_ingestor_for_spec(
            &SourceSpec::Path {
                root: "/tmp".to_string(),
                include: vec![],
                exclude: vec![],
            },
            parser_chain(),
            &fetcher,
            &fetcher,
        );
        assert_eq!(ingestor.kind(), IngestorKind::File);
    }

    #[test]
    fn url_spec_builds_url_ingestor() {
        let fetcher = fetch::HttpUrlFetcher::new().unwrap();
        let ingestor = build_ingestor_for_spec(
            &SourceSpec::Url {
                url: "https://example.com".to_string(),
                refresh_interval_secs: None,
            },
            parser_chain(),
            &fetcher,
            &fetcher,
        );
        assert_eq!(ingestor.kind(), IngestorKind::Url);
    }

    #[test]
    fn feed_spec_builds_feed_ingestor() {
        let fetcher = fetch::HttpUrlFetcher::new().unwrap();
        let entry_fetcher = fetch::HttpUrlFetcher::new_public_only().unwrap();
        let ingestor = build_ingestor_for_spec(
            &SourceSpec::Feed {
                url: "https://example.com/feed.xml".to_string(),
                max_entries: None,
                fetch_full_content: true,
                refresh_interval_secs: None,
            },
            parser_chain(),
            &fetcher,
            &entry_fetcher,
        );
        assert_eq!(ingestor.kind(), IngestorKind::Feed);
    }
}
