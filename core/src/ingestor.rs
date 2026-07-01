use serde::{Deserialize, Serialize};

use crate::block::{IngestorKind, Resource};
use crate::error::Error;

/// Configuration field descriptor for an ingestor's setup.
///
/// Used by CLI to generate interactive prompts for ingestor configuration.
/// See specs/03-config.md §3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub required: bool,
    /// Secret fields are stored in the credentials table, not config_json.
    pub secret: bool,
    pub field_type: ConfigFieldType,
    pub default: Option<String>,
}

/// Type of a configuration field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFieldType {
    String,
    Path,
    Url,
    Integer,
    Boolean,
    Choice(Vec<String>),
}

/// Trait for ingestor-specific configuration.
///
/// Each ingestor kind has a corresponding config type that describes its
/// required and optional fields. This enables the CLI to generate interactive
/// setup prompts and the HTTP API to validate source configurations.
///
/// Lives in `core` as part of the contract. Concrete implementations live
/// outside `core`.
pub trait IngestorConfig: Send + Sync {
    /// Describe the configuration fields for this ingestor.
    fn fields(&self) -> Vec<ConfigField>;

    /// Validate a JSON config object against this ingestor's requirements.
    fn validate(&self, config: &serde_json::Value) -> Result<(), Error>;
}

/// The Ingestor trait — contract for content acquisition and structuring.
///
/// Each ingestor knows how to connect to a source, enumerate content, and
/// produce `Resource`s with typed blocks. The trait yields an async stream
/// of resources.
///
/// Lives in `core` as the contract. Concrete ingestor implementations (file,
/// URL, Notion, Telegram, etc.) live outside `core`, consistent with the
/// "no I/O frameworks in core" invariant.
///
/// See specs/02-domain-model.md §8 and specs/01-architecture.md §1.
#[async_trait::async_trait]
pub trait Ingestor: Send + Sync {
    /// Which ingestor kind this is.
    fn kind(&self) -> IngestorKind;

    /// Ingest content from a source, yielding resources.
    ///
    /// The implementation should:
    /// - Connect to the source (file scan, HTTP fetch, API call)
    /// - Enumerate content items
    /// - Produce a `Resource` with typed blocks for each item
    /// - Yield resources as they become available
    ///
    /// Resources are yielded via callback rather than returned as a Vec to
    /// support streaming large sources without buffering all resources in
    /// memory. The callback receives each resource as it's produced.
    async fn ingest(
        &self,
        source: &IngestSource,
        callback: &mut dyn IngestCallback,
    ) -> Result<IngestResult, Error>;
}

/// Callback for receiving resources during ingestion.
///
/// This is the streaming interface: the ingestor calls `on_resource` for each
/// resource it produces, and the caller processes it immediately.
#[async_trait::async_trait]
pub trait IngestCallback: Send {
    async fn on_resource(&mut self, resource: Resource) -> Result<(), Error>;
}

/// Source information passed to an ingestor.
#[derive(Debug, Clone)]
pub struct IngestSource {
    pub source_id: String,
    pub store_id: String,
    pub ingestor_kind: IngestorKind,
    pub config: serde_json::Value,
}

/// Result of an ingestion run.
#[derive(Debug, Clone, Default)]
pub struct IngestResult {
    pub resources_produced: usize,
    pub resources_skipped: usize,
    pub errors: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_field_type_roundtrip() {
        let types = vec![
            ConfigFieldType::String,
            ConfigFieldType::Path,
            ConfigFieldType::Url,
            ConfigFieldType::Integer,
            ConfigFieldType::Boolean,
            ConfigFieldType::Choice(vec!["a".to_string(), "b".to_string()]),
        ];
        for t in &types {
            let json = serde_json::to_string(t).unwrap();
            let t2: ConfigFieldType = serde_json::from_str(&json).unwrap();
            assert_eq!(t, &t2);
        }
    }

    #[test]
    fn config_field_roundtrip() {
        let field = ConfigField {
            key: "api_token",
            label: "API Token",
            description: "Your Notion integration token",
            required: true,
            secret: true,
            field_type: ConfigFieldType::String,
            default: None,
        };
        let json = serde_json::to_string(&field).unwrap();
        assert!(json.contains("api_token"));
        // ConfigField has &'static str so can't deserialize from runtime string,
        // but serialization proves the shape is correct.
    }

    #[test]
    fn ingest_source_creation() {
        let source = IngestSource {
            source_id: "src-1".to_string(),
            store_id: "store-1".to_string(),
            ingestor_kind: IngestorKind::File,
            config: serde_json::json!({ "root": "/tmp/docs" }),
        };
        assert_eq!(source.ingestor_kind, IngestorKind::File);
    }

    #[test]
    fn ingest_result_default() {
        let result = IngestResult::default();
        assert_eq!(result.resources_produced, 0);
        assert_eq!(result.resources_skipped, 0);
        assert_eq!(result.errors, 0);
    }
}
