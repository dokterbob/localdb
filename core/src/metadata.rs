use serde::{Deserialize, Serialize};

/// Dublin Core Metadata Element Set 1.1 (DCMES), all 15 elements.
///
/// Base metadata shared by all resource kinds.
/// See specs/02-domain-model.md §7.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DublinCoreMetadata {
    pub title: Option<String>,
    pub creator: Vec<String>,
    pub subject: Vec<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub contributor: Vec<String>,
    pub date: Option<String>,
    pub r#type: Option<String>,
    pub format: Option<String>,
    pub identifier: Option<String>,
    pub source: Option<String>,
    pub language: Option<String>,
    pub relation: Vec<String>,
    pub coverage: Option<String>,
    pub rights: Option<String>,
}

/// Document-specific metadata (extends Dublin Core).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocumentMetadata {
    #[serde(flatten)]
    pub dublin_core: DublinCoreMetadata,
    pub page_count: Option<u32>,
    pub word_count: Option<u32>,
}

/// Conversation-specific metadata (extends Dublin Core).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationMetadata {
    #[serde(flatten)]
    pub dublin_core: DublinCoreMetadata,
    pub platform: Option<String>,
    pub message_count: Option<u32>,
    pub date_range: Option<(String, String)>,
}

/// Transcription-specific metadata (extends Dublin Core).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptionMetadata {
    #[serde(flatten)]
    pub dublin_core: DublinCoreMetadata,
    pub duration_ms: Option<u64>,
    pub speakers: Vec<String>,
    pub media_uri: Option<String>,
}

/// Resource metadata enum — discriminated by resource kind.
///
/// Every variant embeds `DublinCoreMetadata` and adds kind-specific fields.
/// See specs/02-domain-model.md §7.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Metadata {
    Document(DocumentMetadata),
    Conversation(ConversationMetadata),
    Transcription(TranscriptionMetadata),
}

impl Metadata {
    /// Access the Dublin Core base fields regardless of variant.
    pub fn dublin_core(&self) -> &DublinCoreMetadata {
        match self {
            Metadata::Document(m) => &m.dublin_core,
            Metadata::Conversation(m) => &m.dublin_core,
            Metadata::Transcription(m) => &m.dublin_core,
        }
    }

    /// Mutable access to the Dublin Core base fields.
    pub fn dublin_core_mut(&mut self) -> &mut DublinCoreMetadata {
        match self {
            Metadata::Document(m) => &mut m.dublin_core,
            Metadata::Conversation(m) => &mut m.dublin_core,
            Metadata::Transcription(m) => &mut m.dublin_core,
        }
    }

    /// Shortcut: the title from Dublin Core.
    pub fn title(&self) -> Option<&str> {
        self.dublin_core().title.as_deref()
    }

    /// Shortcut: the language from Dublin Core.
    pub fn language(&self) -> Option<&str> {
        self.dublin_core().language.as_deref()
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Metadata::Document(DocumentMetadata::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dublin_core_roundtrip() {
        let dc = DublinCoreMetadata {
            title: Some("Test".to_string()),
            creator: vec!["Alice".to_string()],
            date: Some("2026-06-30".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&dc).unwrap();
        let dc2: DublinCoreMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(dc, dc2);
    }

    #[test]
    fn document_metadata_roundtrip() {
        let m = DocumentMetadata {
            dublin_core: DublinCoreMetadata {
                title: Some("My Doc".to_string()),
                language: Some("en".to_string()),
                ..Default::default()
            },
            page_count: Some(42),
            word_count: Some(5000),
        };
        let json = serde_json::to_string(&m).unwrap();
        let m2: DocumentMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn conversation_metadata_roundtrip() {
        let m = ConversationMetadata {
            dublin_core: DublinCoreMetadata {
                title: Some("Chat with Bob".to_string()),
                ..Default::default()
            },
            platform: Some("telegram".to_string()),
            message_count: Some(150),
            date_range: Some(("2026-01-01".to_string(), "2026-06-30".to_string())),
        };
        let json = serde_json::to_string(&m).unwrap();
        let m2: ConversationMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn transcription_metadata_roundtrip() {
        let m = TranscriptionMetadata {
            dublin_core: DublinCoreMetadata::default(),
            duration_ms: Some(3600000),
            speakers: vec!["Alice".to_string(), "Bob".to_string()],
            media_uri: Some("file:///recording.mp3".to_string()),
        };
        let json = serde_json::to_string(&m).unwrap();
        let m2: TranscriptionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn metadata_enum_document_roundtrip() {
        let meta = Metadata::Document(DocumentMetadata {
            dublin_core: DublinCoreMetadata {
                title: Some("Test".to_string()),
                ..Default::default()
            },
            page_count: Some(10),
            word_count: None,
        });
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"kind\":\"document\""));
        let meta2: Metadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, meta2);
    }

    #[test]
    fn metadata_enum_conversation_roundtrip() {
        let meta = Metadata::Conversation(ConversationMetadata {
            dublin_core: DublinCoreMetadata {
                title: Some("Thread #42".to_string()),
                ..Default::default()
            },
            platform: Some("signal".to_string()),
            message_count: None,
            date_range: None,
        });
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"kind\":\"conversation\""));
        let meta2: Metadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, meta2);
    }

    #[test]
    fn metadata_enum_transcription_roundtrip() {
        let meta = Metadata::Transcription(TranscriptionMetadata::default());
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"kind\":\"transcription\""));
        let meta2: Metadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, meta2);
    }

    #[test]
    fn dublin_core_accessor_all_variants() {
        let doc = Metadata::Document(DocumentMetadata {
            dublin_core: DublinCoreMetadata {
                title: Some("Doc".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(doc.dublin_core().title.as_deref(), Some("Doc"));
        assert_eq!(doc.title(), Some("Doc"));

        let conv = Metadata::Conversation(ConversationMetadata {
            dublin_core: DublinCoreMetadata {
                language: Some("nl".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(conv.language(), Some("nl"));

        let trans = Metadata::Transcription(TranscriptionMetadata {
            dublin_core: DublinCoreMetadata {
                title: Some("Recording".to_string()),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(trans.title(), Some("Recording"));
    }

    #[test]
    fn metadata_default_is_document() {
        let meta = Metadata::default();
        assert!(matches!(meta, Metadata::Document(_)));
    }
}
