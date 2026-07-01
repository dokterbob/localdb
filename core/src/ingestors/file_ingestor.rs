//! File-system ingestor: scans a directory tree, parses each file, and emits
//! typed [`Resource`]s.
//!
//! The [`FileIngestor`] implements [`Ingestor`] for `IngestorKind::File`.  It
//! uses the existing `enumerate_path_source` helper from `crate::ingestion`
//! and the `Parser` trait for format-specific parsing.

use crate::block::{IngestorKind, Resource, ResourceKind};
use crate::error::Error;
use crate::ids::document_id;
use crate::ingestion::{enumerate_path_source, now_rfc3339};
use crate::ingestor::{IngestCallback, IngestResult, IngestSource, Ingestor};
use crate::markdown_blocks::{compute_blocks_hash, markdown_to_blocks};
use crate::metadata::{DocumentMetadata, DublinCoreMetadata, Metadata};
use crate::parser::{Parser, Probe};
use crate::uri::Uri;

/// File-system ingestor.
///
/// Reads a directory tree from `source.config["root"]`, optionally filtered by
/// `source.config["include"]` (array of glob patterns) and
/// `source.config["exclude"]` (array of glob patterns).
pub struct FileIngestor {
    /// The parser chain to use for format detection and extraction.
    pub parser: Box<dyn Parser>,
}

impl FileIngestor {
    /// Create a new `FileIngestor` with the given parser chain.
    pub fn new(parser: Box<dyn Parser>) -> Self {
        Self { parser }
    }
}

#[async_trait::async_trait]
impl Ingestor for FileIngestor {
    fn kind(&self) -> IngestorKind {
        IngestorKind::File
    }

    async fn ingest(
        &self,
        source: &IngestSource,
        callback: &mut dyn IngestCallback,
    ) -> Result<IngestResult, Error> {
        // Extract configuration from the JSON config.
        let root = source
            .config
            .get("root")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::InvalidRequest {
                message: "FileIngestor: missing required config field 'root'".to_string(),
            })?;

        let include: Vec<String> = source
            .config
            .get("include")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let exclude: Vec<String> = source
            .config
            .get("exclude")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let files = enumerate_path_source(root, &include, &exclude)?;

        let mut result = IngestResult::default();

        for file in &files {
            let bytes = match std::fs::read(&file.path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(path = %file.path.display(), "FileIngestor: failed to read file: {}", e);
                    result.errors += 1;
                    continue;
                }
            };

            let path_hint = file.path.to_str();
            let probe = Probe::new(&bytes, path_hint, None);

            let parsed = match self.parser.parse(&probe) {
                Ok(Some(doc)) => doc,
                Ok(None) => {
                    result.resources_skipped += 1;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(uri = %file.uri, "FileIngestor: parser error: {}", e);
                    result.errors += 1;
                    continue;
                }
            };

            let blocks = markdown_to_blocks(&parsed.markdown);
            let hash = compute_blocks_hash(&blocks);
            let res_id = document_id(&file.uri, &hash);
            let now = now_rfc3339();

            // Convert parser::DocumentMetadata → metadata::DublinCoreMetadata
            let dc = parser_meta_to_dc(&parsed.metadata);
            let title = parsed.title.clone().or_else(|| dc.title.clone());

            let resource = Resource {
                id: res_id,
                store_id: source.store_id.clone(),
                source_id: source.source_id.clone(),
                ingestor_kind: IngestorKind::File,
                resource_kind: ResourceKind::Document,
                uri: Uri::parse(&file.uri).ok_or_else(|| Error::Internal {
                    message: format!("FileIngestor: invalid URI '{}'", file.uri),
                    correlation_id: "file_ingestor_uri".to_string(),
                })?,
                external_id: None,
                external_etag: None,
                content_hash: hash,
                title,
                mime: None,
                metadata: Metadata::Document(DocumentMetadata {
                    dublin_core: dc,
                    ..Default::default()
                }),
                added_at: now.clone(),
                modified_at: now,
                thread_id: None,
                channel: None,
                participants: vec![],
                origin_store: source.store_id.clone(),
                policy_version: "v1".to_string(),
                share_path: None,
                extractor_version: "1.0".to_string(),
                blocks,
            };

            callback.on_resource(resource).await?;
            result.resources_produced += 1;
        }

        Ok(result)
    }
}

/// Convert `crate::parser::DocumentMetadata` to `crate::metadata::DublinCoreMetadata`.
pub(crate) fn parser_meta_to_dc(meta: &crate::parser::DocumentMetadata) -> DublinCoreMetadata {
    DublinCoreMetadata {
        title: meta.title.clone(),
        creator: meta.creator.clone(),
        subject: meta.subject.clone(),
        description: meta.description.clone(),
        publisher: meta.publisher.clone(),
        contributor: meta.contributor.clone(),
        date: meta.date.clone(),
        r#type: meta.r#type.clone(),
        format: meta.format.clone(),
        identifier: meta.identifier.clone(),
        source: meta.source.clone(),
        language: meta.language.clone(),
        relation: meta.relation.clone(),
        coverage: meta.coverage.clone(),
        rights: meta.rights.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Block, BlockKind};
    use crate::markdown_blocks::compute_blocks_hash;
    use crate::parser::{ChainParser, ParsedDocument};

    #[test]
    fn compute_blocks_hash_includes_kind_and_separator() {
        // Fix 1: hash now includes block kind and uses NUL separator between blocks.
        // "heading:Title\x00paragraph:Body." is the canonical input.
        let blocks = vec![
            Block {
                seq: 0,
                kind: BlockKind::Heading { level: 1 },
                text: "Title".to_string(),
                location: None,
            },
            Block {
                seq: 1,
                kind: BlockKind::Paragraph,
                text: "Body.".to_string(),
                location: None,
            },
        ];
        let hash = compute_blocks_hash(&blocks);
        let expected = crate::ids::content_hash("heading:Title\x00paragraph:Body.");
        assert_eq!(hash, expected);
    }

    #[test]
    fn compute_blocks_hash_structural_change_yields_different_hash() {
        // A paragraph→heading change with the same text must produce a different hash.
        let blocks_para = vec![Block {
            seq: 0,
            kind: BlockKind::Paragraph,
            text: "Same text".to_string(),
            location: None,
        }];
        let blocks_heading = vec![Block {
            seq: 0,
            kind: BlockKind::Heading { level: 1 },
            text: "Same text".to_string(),
            location: None,
        }];
        assert_ne!(
            compute_blocks_hash(&blocks_para),
            compute_blocks_hash(&blocks_heading),
            "paragraph and heading with same text must have different hashes"
        );
    }

    /// A minimal parser for tests: accepts everything, returns the bytes as Markdown.
    struct AllParser;
    impl Parser for AllParser {
        fn id(&self) -> &'static str {
            "all"
        }
        fn parse(&self, probe: &Probe) -> Result<Option<ParsedDocument>, Error> {
            let text = String::from_utf8_lossy(probe.bytes()).to_string();
            Ok(Some(ParsedDocument {
                markdown: text,
                title: None,
                metadata: crate::parser::DocumentMetadata::default(),
            }))
        }
    }

    #[tokio::test]
    async fn file_ingestor_missing_root_errors() {
        let ingestor = FileIngestor::new(Box::new(ChainParser::new("chain", vec![])));
        let source = IngestSource {
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::File,
            config: serde_json::json!({}),
        };
        struct NullCallback;
        #[async_trait::async_trait]
        impl IngestCallback for NullCallback {
            async fn on_resource(&mut self, _r: Resource) -> Result<(), Error> {
                Ok(())
            }
        }
        let result = ingestor.ingest(&source, &mut NullCallback).await;
        assert!(result.is_err(), "missing root should error");
    }

    #[tokio::test]
    async fn file_ingestor_nonexistent_root_produces_no_resources() {
        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = IngestSource {
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::File,
            config: serde_json::json!({"root": "/nonexistent_path_12345"}),
        };
        struct CollectCallback {
            resources: Vec<Resource>,
        }
        #[async_trait::async_trait]
        impl IngestCallback for CollectCallback {
            async fn on_resource(&mut self, r: Resource) -> Result<(), Error> {
                self.resources.push(r);
                Ok(())
            }
        }
        let mut cb = CollectCallback { resources: vec![] };
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();
        assert_eq!(result.resources_produced, 0);
        assert!(cb.resources.is_empty());
    }

    #[tokio::test]
    async fn file_ingestor_reads_files_from_dir() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let file1 = dir.path().join("a.md");
        let file2 = dir.path().join("b.md");
        std::fs::write(&file1, "# A\n\nContent A.").unwrap();
        std::fs::write(&file2, "# B\n\nContent B.").unwrap();

        let ingestor = FileIngestor::new(Box::new(AllParser));
        let source = IngestSource {
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::File,
            config: serde_json::json!({"root": dir.path().to_str().unwrap()}),
        };

        struct CollectCallback {
            resources: Vec<Resource>,
        }
        #[async_trait::async_trait]
        impl IngestCallback for CollectCallback {
            async fn on_resource(&mut self, r: Resource) -> Result<(), Error> {
                self.resources.push(r);
                Ok(())
            }
        }
        let mut cb = CollectCallback { resources: vec![] };
        let result = ingestor.ingest(&source, &mut cb).await.unwrap();
        assert_eq!(result.resources_produced, 2);
        assert_eq!(cb.resources.len(), 2);

        // Each resource should have blocks.
        for res in &cb.resources {
            assert!(!res.blocks.is_empty(), "resource should have blocks");
            assert_eq!(res.store_id, "store-1");
            assert_eq!(res.source_id, "src-1");
            assert_eq!(res.ingestor_kind, IngestorKind::File);
        }
    }
}
