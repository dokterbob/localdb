//! Per-resource indexing: chunk → embed → upsert (specs/01-architecture.md
//! §1).
//!
//! [`index_resource`] is the post-extraction half — the `Resource` it is
//! handed already has final blocks, metadata, and `content_hash`; all
//! acquisition and extraction I/O happened in the `ingest` crate before
//! `core` ever sees it. It preserves the crash-safe ordering the pipeline
//! has always used: embed before delete, so a write failure leaves any
//! existing document for the URI intact and searchable, and the delete
//! itself is never issued as a call separate from the replacing insert.
//!
//! [`callback::PipelineCallback`] is the `IngestCallback` that drives this
//! per resource as a source streams them — split into its own file purely
//! for size; see that module's own doc comment.

mod callback;

pub(in crate::ingestion) use callback::PipelineCallback;

use crate::block::Resource;
use crate::chunker::{chunk_blocks, CharSizer, ChunkSizer, ChunkerConfig, TokenSizer};
use crate::embedder::DocumentChunks;
use crate::error::Error;
use crate::ingestion::deps::IndexResourceDeps;
use crate::metadata::Metadata;
use crate::store::ChunkRecord;
use crate::types::{Chunk, Provenance, Source, SourceRef};

/// Scale a prose token budget to a character budget (×4) for `CharSizer`.
///
/// Used when the embedder has no local tokenizer: the prose preset's
/// token-denominated `target`/`overlap` are reinterpreted as ~4 chars/token so
/// the character-based splitter approximates the intended token budget. Only the
/// `prose` preset is scaled; `code` already uses a char budget.
///
/// `pub(in crate::ingestion)`, not private: its tests live in the sibling
/// `ingestion::tests` module, which needs to reach it despite not being a
/// descendant of this module.
pub(in crate::ingestion) fn scale_to_chars(config: &ChunkerConfig) -> ChunkerConfig {
    if config.preset != "prose" {
        return config.clone();
    }
    ChunkerConfig {
        preset: config.preset.clone(),
        target_tokens: Some(config.resolved_target_tokens() * 4),
        overlap_tokens: Some(config.resolved_overlap_tokens() * 4),
        window_turns: config.window_turns,
        stride_turns: config.stride_turns,
    }
}

/// Run a fallible, synchronous closure and convert any panic into an `Error::Internal`.
///
/// Any panic in extraction or chunking is downgraded to a per-document error so
/// the ingestion loop can continue with the next file rather than unwinding the
/// whole process.
///
/// The default panic hook is temporarily replaced with a no-op before calling
/// `catch_unwind` to suppress the `thread 'main' panicked at ...` output that
/// the default hook prints to stderr.  This swap is NOT thread-safe (the hook
/// is a global), so callers must ensure no concurrent `catch_panic` calls occur.
/// Currently extraction runs single-threaded, so this is safe.
fn catch_panic<T>(
    label: &str,
    f: impl FnOnce() -> Result<T, Error> + std::panic::UnwindSafe,
) -> Result<T, Error> {
    // Suppress the default panic hook's stderr output for any unexpected
    // third-party parser panic on malformed input. The caller emits a clean
    // WARN line instead. (The PDF path no longer panics — pdf_oxide returns
    // errors — but this stays as belt-and-braces for every parser.)
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(f);
    std::panic::set_hook(prev_hook);

    match result {
        Ok(result) => result,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic payload".to_string());
            Err(Error::Internal {
                message: format!("{label} panicked: {msg}"),
                correlation_id: label.replace(' ', "_"),
            })
        }
    }
}

/// A resource's post-backfill metadata state, plus its derived
/// `metadata_hash` and Dublin Core dates — the single source of truth for
/// both what [`index_resource`] persists and what [`PipelineCallback::on_resource`]
/// compares against, so a resource's title-backfill decision is never made
/// twice with room for the two sites to disagree (issue #176's whole premise:
/// the metadata_hash is comparable across index-time, metadata-update-time,
/// and rehydration-time only if every writer derives it from the *persisted*
/// state via this one function). See specs/04-search-pipeline.md.
struct DerivedResourceState {
    /// `resource.metadata`, with `resource.title` folded into
    /// `dublin_core_mut().title` when the metadata itself carried none.
    metadata: Metadata,
    /// `core::ids::compute_metadata_hash` of `metadata` plus
    /// `resource.external_id`/`external_etag`/`modified_at`.
    metadata_hash: String,
    /// `metadata`'s own Dublin Core `date`, exactly as the source expressed it.
    date_original: Option<String>,
    /// `date_original` normalized via `crate::dates::parse_partial_iso8601`.
    date_parsed: Option<String>,
    /// `resource.modified_at`, unchanged — carried here so the value fed to
    /// `compute_metadata_hash` above is exactly the value later stamped onto
    /// the persisted `ChunkRecord`/`ResourceRecord`, never two separately
    /// read copies.
    modified_at: Option<String>,
}

/// Compute [`DerivedResourceState`] for `resource`. Pure function of
/// `resource` alone — safe to call more than once for the same resource (as
/// [`PipelineCallback::on_resource`] and [`index_resource`] both do, on
/// different branches) since `Metadata` carries no maps and therefore
/// serializes deterministically (see `compute_metadata_hash`'s doc comment).
fn derive_resource_state(resource: &Resource) -> DerivedResourceState {
    // Title propagation: resource.title backfills the metadata's Dublin Core
    // title when the resource's own metadata doesn't already carry one.
    let mut metadata = resource.metadata.clone();
    if metadata.dublin_core().title.is_none() {
        if let Some(title) = &resource.title {
            metadata.dublin_core_mut().title = Some(title.clone());
        }
    }

    // The resource's own claimed date, exactly as the source expressed it —
    // computed once per resource (not redundantly per chunk). Read from
    // `metadata` (post-title-backfill is fine: the Dublin Core date itself
    // is never backfilled) rather than `resource.metadata`, so a future
    // backfill of `dc.date` would be picked up here too.
    let date_original = metadata.dublin_core().date.clone();
    let date_parsed = date_original
        .as_deref()
        .and_then(crate::dates::parse_partial_iso8601);

    let modified_at = resource.modified_at.clone();

    let metadata_hash = crate::ids::compute_metadata_hash(
        &metadata,
        resource.external_id.as_deref(),
        resource.external_etag.as_deref(),
        modified_at.as_deref(),
    );

    DerivedResourceState {
        metadata,
        metadata_hash,
        date_original,
        date_parsed,
        modified_at,
    }
}

/// Compute the effective `ChunkerConfig` for one resource (issue #60; see
/// specs/04-search-pipeline.md §3 "Source preset override").
///
/// - A source whose `source_preset` is anything other than the default
///   `"prose"` is authoritative: `base_chunker` (assumed already resolved for
///   that preset by the caller, e.g. `ChunkerConfig::code()`/`::messages()`
///   plus any store-level overrides) is used **unconditionally**, regardless
///   of what per-file detection would otherwise guess. This is what lets an
///   explicit `code` or `messages` source win over a `.md` file that
///   `preset_for` would otherwise route to `prose`.
/// - A `"prose"` (default) source allows per-file auto-routing: `preset_for`
///   inspects `filename_hint`/`mime`; when it says `"code"`,
///   `ChunkerConfig::code()` defaults are used, otherwise `base_chunker` (the
///   store's configured prose chunker) is used.
///
/// `pub(in crate::ingestion)`, not private: its tests live in the sibling
/// `ingestion::tests` module, which needs to reach it despite not being a
/// descendant of this module.
pub(in crate::ingestion) fn effective_chunker_config(
    source_preset: &str,
    base_chunker: &ChunkerConfig,
    filename_hint: Option<&str>,
    mime: Option<&str>,
) -> ChunkerConfig {
    if source_preset != "prose" {
        return base_chunker.clone();
    }
    use crate::chunker::preset_for;
    if preset_for(filename_hint, mime) == "code" {
        ChunkerConfig::code()
    } else {
        base_chunker.clone()
    }
}

/// Derive a filename hint from a resource's URI: its last path segment, if any.
///
/// Used by [`effective_chunker_config`]'s per-file auto-routing. Non-hierarchical
/// or extension-less URIs (e.g. `notion://page/abc123`) simply yield `None`,
/// falling through to mime-based or default (`prose`) routing.
fn filename_hint_from_uri(uri: &crate::uri::Uri) -> Option<String> {
    let last = uri.as_url().path_segments()?.next_back()?;
    if last.is_empty() {
        None
    } else {
        Some(last.to_string())
    }
}

/// Index a single already-built `Resource`: the post-extraction half of the
/// pipeline (preset gate → chunk → embed → upsert).
///
/// Crash-safe A6 ordering: chunking and embedding happen first (read-only /
/// reversible); only once embedding has succeeded is `replaces_resource_id`
/// threaded into a single
/// `upsert_chunks_and_blocks` call, so a write failure leaves any existing
/// document for this URI intact and searchable (issue #79) — the replace
/// delete is never issued as a separate call.
///
/// The skip-check (unchanged content) is the caller's responsibility (see
/// `PipelineCallback` below) — this function always (re)indexes; `resource`'s
/// blocks, metadata, and `content_hash` must already be final.
///
/// Returns [`IndexOutcome::Written`] with the number of chunks written and
/// the persisted metadata_hash, or [`IndexOutcome::Empty`] if the resource
/// produced no chunks at all.
pub async fn index_resource(
    resource: &Resource,
    source: &Source,
    replaces_resource_id: Option<&str>,
    deps: &IndexResourceDeps<'_>,
) -> Result<IndexOutcome, Error> {
    let token_counter = deps.embedder.token_counter();
    let sizer: Box<dyn ChunkSizer> = match &token_counter {
        Some(f) => Box::new(TokenSizer::new(f.clone())),
        None => Box::new(CharSizer),
    };

    // Preset gate (#60).
    let filename_hint = filename_hint_from_uri(&resource.uri);
    let effective_chunker = effective_chunker_config(
        &source.source_preset,
        &deps.config.chunker,
        filename_hint.as_deref(),
        resource.mime.as_deref(),
    );

    let chunker_cfg = if token_counter.is_none() {
        scale_to_chars(&effective_chunker)
    } else {
        effective_chunker
    };

    let chunk_outputs = catch_panic(
        "chunk",
        std::panic::AssertUnwindSafe(|| {
            chunk_blocks(&resource.id, &resource.blocks, &chunker_cfg, sizer.as_ref())
        }),
    )?;

    if chunk_outputs.is_empty() {
        // #185 — the sink's invariant: an empty replacement writes nothing AND
        // deletes nothing.
        //
        // This arm used to `delete_by_resource(replaces_resource_id)`, on the
        // reading that a resource which now chunks to nothing is a document
        // that has become empty. But `index_resource` cannot distinguish
        // "this file is legitimately empty now" from "extraction produced
        // nothing this run" (a scanned PDF with no text layer, a parser
        // regression, an HTML page whose body failed to render) — and only the
        // first is evidence the content is gone. Guarding this at each
        // ingestor (as PR #170 did for url/feed) leaves every future connector
        // one oversight away from silently erasing a URI's content, so the
        // rule lives here, where nothing can bypass it.
        //
        // The escape hatch for a genuinely empty file is clean and already
        // exists: delete the file, and the delete-sweep removes it normally.
        tracing::warn!(
            uri = %resource.uri,
            "resource produced no chunks — keeping any previously indexed \
             content for this URI (delete the source item if it is really gone)"
        );
        return Ok(IndexOutcome::Empty);
    }

    // Embed BEFORE any delete (A6) — see module doc comment above.
    // `document_context` is built from the resource's block texts in order
    // (the new `Resource` shape carries blocks, not a flat Markdown string).
    let document_context = resource
        .blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let doc_chunks = DocumentChunks {
        document_context,
        chunks: chunk_outputs.iter().map(|c| c.text.clone()).collect(),
    };

    let embedded = deps.embedder.embed_documents(vec![doc_chunks]).await?;

    // Guard: the embedder must return exactly one EmbeddedDocument (one per
    // input document), and that document must have exactly one vector per
    // chunk. A length mismatch indicates a malformed embedder response (F4).
    if embedded.len() != 1 {
        return Err(Error::Internal {
            message: format!(
                "embedder returned {} EmbeddedDocuments for 1 input document",
                embedded.len()
            ),
            correlation_id: "embed_count_mismatch".to_string(),
        });
    }
    let embeddings = &embedded[0];
    if embeddings.len() != chunk_outputs.len() {
        return Err(Error::Internal {
            message: format!(
                "embedder returned {} vectors for {} chunks",
                embeddings.len(),
                chunk_outputs.len()
            ),
            correlation_id: "embed_chunk_count_mismatch".to_string(),
        });
    }

    let provenance = Provenance {
        origin_store: deps.config.store_id.clone(),
        source_ref: SourceRef {
            id: resource.source_id.clone(),
            kind: resource.ingestor_kind.as_str().to_string(),
        },
        // Acquisition time, i.e. when *our store* got hold of this resource —
        // `added_at`, never `modified_at` (which for a feed entry is the
        // feed's own claim about the content's age). The libsql backend binds
        // this to `resources.added_at`, the column `MetadataFilter::
        // DateAfter`/`DateBefore` (`DateAxis::Added`) filter on and every
        // citation reports. See specs/02-domain-model.md §4.
        fetched_at: resource.added_at.clone(),
        content_hash: resource.content_hash.clone(),
        share_path: vec![],
    };

    // Post-backfill metadata, its persisted-state hash, and derived dates —
    // computed via the one function `on_resource`'s skip-check and
    // metadata-only-update path also call, so the two never disagree about
    // what a resource's persisted state is (issue #176). See
    // `DerivedResourceState`'s doc comment.
    let derived = derive_resource_state(resource);
    let record_metadata = derived.metadata;
    let date_original = derived.date_original;
    let date_parsed = derived.date_parsed;
    let modified_at = derived.modified_at;

    // Page lookup for paginated formats (#103): block seq → location.page,
    // copied onto each chunk record from its originating block.
    let page_by_seq: std::collections::HashMap<u32, u32> = resource
        .blocks
        .iter()
        .filter_map(|b| {
            b.location
                .as_ref()
                .and_then(|loc| loc.page)
                .map(|page| (b.seq, page))
        })
        .collect();

    let mut records = Vec::with_capacity(chunk_outputs.len());
    for (chunk_out, embedding) in chunk_outputs.iter().zip(embeddings.iter()) {
        let chunk = Chunk {
            id: chunk_out.id.clone(),
            resource_id: resource.id.clone(),
            store_id: deps.config.store_id.clone(),
            text: chunk_out.text.clone(),
            span: chunk_out.span.clone(),
            heading_path: chunk_out.heading_path.clone(),
            policy_version: deps.config.policy_version.clone(),
            provenance: provenance.clone(),
            window_block_seqs: chunk_out.window_block_seqs.clone(),
        };

        let mut record = ChunkRecord::from_chunk(
            &chunk,
            embedding.clone(),
            resource.uri.as_str().to_string(),
            resource.mime.clone(),
            record_metadata.clone(),
        );
        record.block_seq = chunk_out.block_seq;
        record.seq_in_block = chunk_out.seq_in_block;
        record.block_kind = chunk_out.block_kind.clone();
        record.page = page_by_seq.get(&chunk_out.block_seq).copied();
        // The resource's own claimed modification time — distinct from
        // `fetched_at`/`provenance.fetched_at` (acquisition time, stamped by
        // `from_chunk` above). Normalized (`Some("")` → `None`) via
        // `derive_resource_state`, so this always matches what
        // `derived.metadata_hash` was computed over. See
        // specs/02-domain-model.md §2.
        record.modified_at = modified_at.clone();
        record.date_original = date_original.clone();
        record.date_parsed = date_parsed.clone();
        record.external_id = resource.external_id.clone();
        record.external_etag = resource.external_etag.clone();
        records.push(record);
    }

    let written = records.len();
    deps.store
        .upsert_chunks_and_blocks(
            &deps.config.store_id,
            &resource.id,
            records,
            &resource.blocks,
            replaces_resource_id,
            resource.external_last_modified.as_deref(),
        )
        .await?;

    Ok(IndexOutcome::Written(written, derived.metadata_hash))
}

/// What [`index_resource`] did with a resource.
///
/// `Empty` is a distinct outcome rather than `Written(0)` because the caller
/// must treat it differently (#185): a resource that chunked to nothing is
/// *not* an indexed document, and recording it as one — bumping
/// `docs_indexed`, upserting its hash into the `DocumentIndex` — is what
/// turned "this file extracted to nothing" into "this file's indexed content
/// is gone."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOutcome {
    /// The resource was chunked, embedded, and written: chunk count, plus
    /// the `metadata_hash` this write persisted (issue #176) — threaded out
    /// rather than recomputed at the call site, so the value the caller
    /// stamps into its `DocumentIndex` is guaranteed to be exactly what this
    /// call persisted, not a second, separately-computed guess at it.
    Written(usize, String),
    /// The resource produced no chunks. Nothing was written, and — the
    /// invariant this type exists to carry — nothing was deleted either.
    Empty,
}
