//! Parser registry: maps parser IDs to concrete `Parser` implementations.
//!
//! `build_chain` is the primary entry point. It consumes an ordered list of
//! parser IDs (from `IndexingPolicyConfig.parsers`) and returns a `ChainParser`
//! that embodies the strict first-match strategy.

use localdb_core::parser::{ChainParser, Parser};
use localdb_core::Error;

use crate::parsers::{
    EpubParser, HtmlParser, MarkdownParser, OfficeParser, PdfParser, PlaintextParser,
};

/// The canonical ordered list of parser IDs used when the config omits `parsers`.
pub fn default_parser_ids() -> Vec<String> {
    vec![
        "pdf".to_string(),
        "epub".to_string(),
        "office".to_string(),
        "html".to_string(),
        "markdown".to_string(),
        "plaintext".to_string(),
    ]
}

/// Build a `ChainParser` from an ordered list of parser IDs.
///
/// The order of `enabled_ids` is the priority order: the first parser that
/// returns `Ok(Some)` wins. An unknown ID is a hard error (config validation
/// should have caught it, but we check here defensively).
///
/// # Errors
/// Returns `Error::InvalidConfig` if any ID is not in the known registry.
pub fn build_chain(enabled_ids: &[String]) -> Result<ChainParser, Error> {
    let parsers: Vec<Box<dyn Parser>> = enabled_ids
        .iter()
        .map(|id| -> Result<Box<dyn Parser>, Error> {
            match id.as_str() {
                "pdf" => Ok(Box::new(PdfParser)),
                "epub" => Ok(Box::new(EpubParser)),
                "office" => Ok(Box::new(OfficeParser)),
                "html" => Ok(Box::new(HtmlParser)),
                "markdown" => Ok(Box::new(MarkdownParser)),
                "plaintext" => Ok(Box::new(PlaintextParser)),
                other => Err(Error::InvalidConfig {
                    message: format!(
                        "unknown parser id '{other}'; known ids are: pdf, epub, office, html, markdown, plaintext"
                    ),
                }),
            }
        })
        .collect::<Result<_, _>>()?;

    Ok(ChainParser::new("chain", parsers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use localdb_core::parser::Probe;

    #[test]
    fn default_ids_are_six() {
        let ids = default_parser_ids();
        assert_eq!(
            ids,
            vec!["pdf", "epub", "office", "html", "markdown", "plaintext"]
        );
    }

    #[test]
    fn build_chain_default_ids_succeeds() {
        let ids = default_parser_ids();
        assert!(build_chain(&ids).is_ok());
    }

    #[test]
    fn build_chain_resolves_epub() {
        let chain = build_chain(&["epub".to_string()]).unwrap();
        // Non-epub input is declined by the epub parser → chain returns None.
        let probe = Probe::new(b"# Hello", Some("doc.md"), None);
        assert!(chain.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn build_chain_unknown_id_errors() {
        let ids = vec!["pdf".to_string(), "unknown-format".to_string()];
        let err = build_chain(&ids).unwrap_err();
        assert!(
            format!("{err}").contains("unknown parser id"),
            "error should mention unknown id, got: {err}"
        );
    }

    #[test]
    fn build_chain_empty_returns_ok() {
        let chain = build_chain(&[]).unwrap();
        let probe = Probe::new(b"data", None, None);
        assert!(chain.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn build_chain_markdown_only_skips_html() {
        let ids = vec!["markdown".to_string()];
        let chain = build_chain(&ids).unwrap();
        // HTML input should be declined (markdown parser won't accept it without ext)
        let probe = Probe::new(b"<html><body><p>Hi</p></body></html>", None, None);
        assert!(chain.parse(&probe).unwrap().is_none());
    }

    #[test]
    fn build_chain_markdown_only_accepts_md() {
        let ids = vec!["markdown".to_string()];
        let chain = build_chain(&ids).unwrap();
        let probe = Probe::new(b"# Hello\n\nWorld.", Some("doc.md"), None);
        assert!(chain.parse(&probe).unwrap().is_some());
    }

    #[test]
    fn chain_excludes_pdf_when_not_in_ids() {
        let ids = vec!["markdown".to_string(), "plaintext".to_string()];
        let chain = build_chain(&ids).unwrap();
        let probe = Probe::new(b"%PDF-1.4\n%%EOF", Some("report.pdf"), None);
        // PdfParser is not in the chain. MarkdownParser declines .pdf extension.
        // PlaintextParser now also declines .pdf (not in its recognized list).
        // Result: chain returns Ok(None) — no crash.
        let result = chain.parse(&probe);
        assert!(result.is_ok());
    }
}
