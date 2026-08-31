use serde::{Deserialize, Serialize};

/// Dublin Core Metadata Element Set 1.1 (DCMES), all 15 elements.
///
/// Base metadata shared by all resource kinds.
/// See specs/02-domain-model.md §7.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DublinCoreMetadata {
    pub title: Option<String>,
    #[serde(default)]
    pub creator: Vec<String>,
    #[serde(default)]
    pub subject: Vec<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    #[serde(default)]
    pub contributor: Vec<String>,
    pub date: Option<String>,
    /// Provenance of `date`: which extraction site stamped it (e.g.
    /// `"pdf-info"`, `"xmp"`, `"epub-opf"`, `"feed-entry"`,
    /// `"office-core-properties"`, `"html-json-ld"`, `"html-meta"`,
    /// `"front-matter"`). `#[serde(skip_serializing_if)]` so documents
    /// without a stamped date_source (i.e. every document indexed before
    /// this field existed) keep serializing byte-identical to before — a
    /// missing skip attribute here would change `metadata_hash` for the
    /// whole corpus on the first post-upgrade index run. See
    /// specs/02-domain-model.md §7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_source: Option<String>,
    pub r#type: Option<String>,
    pub format: Option<String>,
    pub identifier: Option<String>,
    pub source: Option<String>,
    pub language: Option<String>,
    #[serde(default)]
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
    #[serde(default)]
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

/// What a connector knows about a resource independently of the resource's
/// own content — a feed entry's title, authors, publication date, and the
/// feed that carried it.
///
/// Kept apart from [`DublinCoreMetadata`] because the two merge
/// asymmetrically, and that asymmetry is the whole point: `title_fallback`
/// only fills a gap the extraction left, while `creator`, `date` and
/// `provenance_source` overwrite whatever the extraction produced. A feed
/// knows the author of an entry better than the linked page's markup does;
/// it does not know the page's title better than the page does.
///
/// It exists as a `core` type rather than an ingestor-private one so the
/// same merge runs on both sides of a conditional GET: at index time an
/// ingestor applies it to freshly parsed metadata, and on a 304 —
/// [`crate::ingestor::IngestCallback::on_metadata_refreshed`] — the pipeline
/// applies it to the persisted metadata, since a 304 returns no body to
/// re-parse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataEnrichment {
    /// Title to use only when the merged metadata still carries none.
    pub title_fallback: Option<String>,
    /// Author name(s), replacing whatever the extraction found when
    /// non-empty.
    pub creator: Vec<String>,
    /// Date claim (RFC 3339 or a Dublin Core partial), replacing the
    /// extracted `date`.
    pub date: Option<String>,
    /// Provenance stamp for `date`, written in the same breath as `date` so
    /// an extraction's own `date_source` can never survive attached to a
    /// value it did not produce.
    pub date_source: Option<String>,
    /// Value for `DublinCoreMetadata::source` — the connector the resource
    /// was reached through (e.g. the owning feed's URL).
    pub provenance_source: Option<String>,
}

impl MetadataEnrichment {
    /// Whether this enrichment carries anything at all. A connector with no
    /// out-of-band knowledge of its resources (a plain URL fetch) produces
    /// an empty one, and applying it is a guaranteed no-op.
    pub fn is_empty(&self) -> bool {
        self.title_fallback.is_none()
            && self.creator.is_empty()
            && self.date.is_none()
            && self.provenance_source.is_none()
    }

    /// Layer this enrichment onto `dc` in place.
    ///
    /// Idempotent, and a pure function of `(self, dc)` — applying it to
    /// already-enriched metadata reproduces that same metadata, which is
    /// what lets the 304 path apply it to persisted state and compare the
    /// result for equality rather than writing blindly.
    pub fn apply_to(&self, dc: &mut DublinCoreMetadata) {
        if dc.title.is_none() {
            dc.title = self.title_fallback.clone();
        }
        if !self.creator.is_empty() {
            dc.creator = self.creator.clone();
        }
        if let Some(date) = &self.date {
            dc.date = Some(date.clone());
            dc.date_source = self.date_source.clone();
        }
        if let Some(source) = &self.provenance_source {
            dc.source = Some(source.clone());
        }
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

    #[test]
    fn dublin_core_missing_vec_fields_deserialize_as_empty() {
        let json = r#"{"title": "Test"}"#;
        let dc: DublinCoreMetadata =
            serde_json::from_str(json).expect("should deserialize with missing Vec fields");
        assert_eq!(dc.title.as_deref(), Some("Test"));
        assert!(dc.creator.is_empty());
        assert!(dc.subject.is_empty());
        assert!(dc.contributor.is_empty());
        assert!(dc.relation.is_empty());
    }

    /// Pin: a `DublinCoreMetadata` with `date_source: None` must serialize to
    /// the exact same JSON as it did before `date_source` existed — no
    /// `"date_source"` key at all, and no shift in any other field's
    /// position. Without `skip_serializing_if` on the new field, every
    /// existing document's `metadata_hash` (which hashes this JSON, see
    /// `core::ids::compute_metadata_hash`) would change on the first
    /// post-upgrade index run, forcing a full-corpus reindex.
    #[test]
    fn date_source_none_serializes_identically_to_before_the_field_existed() {
        let dc = DublinCoreMetadata {
            title: Some("Pre-upgrade Doc".to_string()),
            creator: vec!["Alice".to_string()],
            date: Some("2020-01-01".to_string()),
            language: Some("en".to_string()),
            date_source: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&dc).unwrap();
        assert!(
            !json.contains("date_source"),
            "date_source: None must not appear in the serialized JSON at all: {json}"
        );

        // The pre-existing field order/shape, hand-pinned as an exact string
        // (not a Value comparison, which would ignore key order): reordering
        // fields, or making the new field always-present, would change this
        // and, with it, every pre-upgrade document's metadata_hash.
        let expected = concat!(
            r#"{"title":"Pre-upgrade Doc","creator":["Alice"],"subject":[],"#,
            r#""description":null,"publisher":null,"contributor":[],"#,
            r#""date":"2020-01-01","type":null,"format":null,"identifier":null,"#,
            r#""source":null,"language":"en","relation":[],"coverage":null,"rights":null}"#,
        );
        assert_eq!(json, expected);
    }

    // ------------------------------------------------------------------
    // MetadataEnrichment
    // ------------------------------------------------------------------

    fn feed_enrichment() -> MetadataEnrichment {
        MetadataEnrichment {
            title_fallback: Some("Feed's title".to_string()),
            creator: vec!["Jane Doe".to_string()],
            date: Some("2026-02-09T00:00:00Z".to_string()),
            date_source: Some("feed-entry".to_string()),
            provenance_source: Some("https://feed.example.com/feed.xml".to_string()),
        }
    }

    #[test]
    fn enrichment_title_only_fills_a_gap() {
        let mut dc = DublinCoreMetadata {
            title: Some("The page's own title".to_string()),
            ..Default::default()
        };
        feed_enrichment().apply_to(&mut dc);
        assert_eq!(
            dc.title.as_deref(),
            Some("The page's own title"),
            "a connector never knows a page's title better than the page does"
        );

        let mut dc = DublinCoreMetadata::default();
        feed_enrichment().apply_to(&mut dc);
        assert_eq!(dc.title.as_deref(), Some("Feed's title"));
    }

    #[test]
    fn enrichment_overwrites_creator_date_and_source() {
        let mut dc = DublinCoreMetadata {
            creator: vec!["Extracted Author".to_string()],
            date: Some("1999-01-01".to_string()),
            date_source: Some("html-json-ld".to_string()),
            source: Some("https://elsewhere.example".to_string()),
            ..Default::default()
        };
        feed_enrichment().apply_to(&mut dc);
        assert_eq!(dc.creator, vec!["Jane Doe".to_string()]);
        assert_eq!(dc.date.as_deref(), Some("2026-02-09T00:00:00Z"));
        assert_eq!(
            dc.date_source.as_deref(),
            Some("feed-entry"),
            "date and its provenance move together, or the stamp becomes a lie \
             about where the value came from"
        );
        assert_eq!(
            dc.source.as_deref(),
            Some("https://feed.example.com/feed.xml")
        );
    }

    #[test]
    fn enrichment_leaves_absent_claims_alone() {
        let original = DublinCoreMetadata {
            title: Some("T".to_string()),
            creator: vec!["Extracted Author".to_string()],
            date: Some("1999-01-01".to_string()),
            date_source: Some("html-json-ld".to_string()),
            source: Some("https://elsewhere.example".to_string()),
            description: Some("A description the connector knows nothing about".to_string()),
            ..Default::default()
        };
        let mut dc = original.clone();
        MetadataEnrichment::default().apply_to(&mut dc);
        assert_eq!(dc, original, "an empty enrichment must be a total no-op");
    }

    #[test]
    fn enrichment_is_idempotent() {
        // The 304 path applies this to already-enriched persisted state and
        // compares the result for equality; a non-idempotent merge would
        // make every 304 look like a change and rewrite the row forever.
        let mut once = DublinCoreMetadata::default();
        feed_enrichment().apply_to(&mut once);
        let mut twice = once.clone();
        feed_enrichment().apply_to(&mut twice);
        assert_eq!(once, twice);
    }

    #[test]
    fn enrichment_is_empty_ignores_date_source() {
        assert!(MetadataEnrichment::default().is_empty());
        assert!(!feed_enrichment().is_empty());
        // date_source alone is meaningless — it is provenance for a `date`
        // that isn't there, and applying it would change nothing.
        assert!(MetadataEnrichment {
            date_source: Some("feed-entry".to_string()),
            ..Default::default()
        }
        .is_empty());
    }
}
