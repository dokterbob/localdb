//! [`PipelineCallback`]: the `IngestCallback` implementation that streams an
//! `Ingestor`'s resources into [`super::index_resource`] one at a time.
//!
//! Split out from `super` (rather than folded into it) purely on size: this
//! is one large trait impl, and keeping it in its own file means
//! `super::index_resource` and its small helpers stay reviewable without
//! scrolling past several hundred lines of callback plumbing to reach them.
//! `pub(in crate::ingestion)` throughout, not `pub(super)`: `run_source_ingestion`
//! (in the `ingestion` module itself, two levels up) constructs and
//! destructures this struct by name, so its fields need to reach that far,
//! but no farther — nothing outside `ingestion` ever sees a `PipelineCallback`.

use crate::block::Resource;
use crate::embedder::Embedder;
use crate::error::Error;
use crate::ingestion::deps::{DocumentIndex, DocumentRecord, FetchMetadata, IndexResourceDeps};
use crate::ingestion::liveness::FEED_LIVENESS_BATCH_LIMIT;
use crate::ingestion::{now_rfc3339, IngestionConfig, IngestionResult};
use crate::ingestor::{DueRecheckEntry, IngestCallback, MetadataWriteOutcome, SkipReason};
use crate::store::RetrievalStore;
use crate::types::{Source, SourceSpec};
use crate::uri::Uri;

use super::{derive_resource_state, index_resource, IndexOutcome};

/// `IngestCallback` implementation that drives the unified pipeline one
/// `Resource` at a time.
///
/// # The `&mut DocumentIndex`-across-`await` design
///
/// `PipelineCallback` OWNS its dependency references (including
/// `doc_index: &'a mut DocumentIndex`) as plain struct fields rather than
/// threading them through method parameters. `#[async_trait]` desugars
/// `on_resource`/`on_discovered`/`on_skipped` into methods returning
/// `Pin<Box<dyn Future<Output = ...> + Send + 'async_trait>>` tied to
/// `&'async_trait mut self`. Since the mutable borrow of `DocumentIndex` lives
/// entirely *inside* that per-call future (never held across separate calls,
/// never stored anywhere else), there is no conflict: each call reborrows
/// `self.doc_index` for its own duration and releases it when the future
/// resolves — ordinary NLL reborrowing, not a lifetime fight. `run_source_ingestion`
/// hands `PipelineCallback` its own `&mut DocumentIndex` (from
/// `SourceIngestionDeps`) for the lifetime of the `ingestor.ingest(...)` call
/// only; once that call returns, `callback` is destructured and `doc_index` is
/// used directly again for the delete-sweep. No interior mutability
/// (`RefCell`/`Mutex`) is needed — the fix for the "known risk" flagged for
/// this ticket was simply to give the callback ownership of the dependency
/// *references* up front, rather than threading `&mut DocumentIndex` through a
/// chain of function parameters that would each need to re-borrow it across an
/// `.await` point.
pub(in crate::ingestion) struct PipelineCallback<'a> {
    pub(in crate::ingestion) source: &'a Source,
    pub(in crate::ingestion) doc_index: &'a mut DocumentIndex,
    pub(in crate::ingestion) store: &'a dyn RetrievalStore,
    pub(in crate::ingestion) embedder: &'a dyn Embedder,
    pub(in crate::ingestion) config: &'a IngestionConfig,
    pub(in crate::ingestion) progress: Option<crate::progress::ProgressSink>,
    pub(in crate::ingestion) result: IngestionResult,
    /// URIs yielded or reported skipped this run — survive the delete-sweep.
    pub(in crate::ingestion) seen: std::collections::HashSet<String>,
    /// URIs the ingestor positively confirmed gone at the origin (404/410
    /// after retry). Deleted unconditionally — see `IngestCallback::on_gone`.
    pub(in crate::ingestion) gone: std::collections::HashSet<String>,
    /// Last total reported via `on_discovered`, if any (0 until then).
    pub(in crate::ingestion) discovered_total: usize,
    /// Running index for `ProgressEvent::DocumentStarted`.
    pub(in crate::ingestion) next_index: usize,
    /// Count of `on_skipped(SkipReason::Error(_))` calls this run — used
    /// only to cross-check the ingestor's own `IngestResult.errors` in
    /// `run_source_ingestion` (see the debug_assert there); NOT folded into
    /// `result.error_count` twice.
    pub(in crate::ingestion) skip_error_count: usize,
    /// `localdb index --refetch`'s cache-busting escape hatch, threaded
    /// straight from `SourceIngestionDeps::refetch`. Bypasses check (c) of
    /// [`Self::recheck_is_due`] — see that method's doc comment.
    pub(in crate::ingestion) refetch: bool,
}

/// What each metadata-refresh hook is refreshing, named once so the read
/// failure, the write failure and the log line for one hook cannot drift into
/// describing it three different ways.
const VALIDATORS: &str = "conditional-GET validators";
const CONNECTOR_METADATA: &str = "source-supplied metadata";
const RECHECK_CLAIM: &str = "the recheck gate's claim comparison";

/// What [`PipelineCallback::merge_metadata_claim`] computes: a connector's
/// claim merged onto a resource's persisted metadata, without writing it
/// anywhere. `metadata_hash` is `core::ids::compute_metadata_hash` over
/// `metadata` plus the claim's `external_id`/`modified_at` and the
/// resource's already-persisted `external_etag` — the same triple
/// `on_metadata_refreshed` compares against `DocumentRecord::metadata_hash`
/// to decide whether anything actually changed.
struct MergedMetadata {
    metadata: crate::metadata::Metadata,
    date_original: Option<String>,
    date_parsed: Option<String>,
    metadata_hash: String,
}

impl PipelineCallback<'_> {
    /// Read the resource row both metadata-refresh hooks rewrite.
    ///
    /// `Err` carries the outcome the caller must return: `Unchanged` for a
    /// missing row — a concurrent delete, the same race the `doc_index` miss
    /// each hook checks first tolerates — and `Failed` for a read error, so a
    /// refresh that could not even read reports as an error rather than a
    /// clean skip.
    ///
    /// `what` names the refresh in the message and selects no behavior. It is
    /// the same constant the caller passes to `persist_metadata_write` a few
    /// lines later, so a read failure and a write failure for one hook can
    /// never describe themselves differently.
    async fn read_persisted_record(
        &self,
        uri: &str,
        resource_id: &str,
        what: &str,
    ) -> Result<crate::store::ResourceRecord, MetadataWriteOutcome> {
        match self
            .store
            .get_resource_record(&self.config.store_id, resource_id)
            .await
        {
            Ok(Some(record)) => Ok(record),
            Ok(None) => Err(MetadataWriteOutcome::Unchanged),
            Err(e) => {
                let msg = format!("error reading resource '{uri}' to refresh {what}: {e}");
                tracing::warn!("{msg}");
                Err(MetadataWriteOutcome::Failed(msg))
            }
        }
    }

    /// The read-only half of a metadata refresh: apply a connector's claim
    /// (`enrichment`, `external_id`, `modified_at`) onto a resource's
    /// *persisted* state — read fresh via [`Self::read_persisted_record`] —
    /// and recompute what its `metadata_hash` would be under that merge,
    /// without writing anything.
    ///
    /// Shared by [`Self::on_metadata_refreshed`], which writes when the
    /// result differs from what `doc_index` has cached, and by
    /// [`Self::recheck_is_due`]'s check (b), which only ever compares — the
    /// same merge specs/04-search-pipeline.md §1 "Recheck gate" describes as
    /// "using the same read-only merge `on_metadata_refreshed` applies when
    /// it writes." Keeping one function under both callers means the two can
    /// never drift into disagreeing about what a feed's current claim
    /// resolves to.
    ///
    /// `Err` carries the same [`MetadataWriteOutcome`]
    /// [`Self::read_persisted_record`] itself produces for a store miss or a
    /// store read error; `read_persisted_record` already logs a read error,
    /// so neither caller logs again for that case.
    async fn merge_metadata_claim(
        &self,
        existing: &DocumentRecord,
        enrichment: &crate::metadata::MetadataEnrichment,
        external_id: Option<&str>,
        modified_at: Option<&str>,
        what: &str,
    ) -> Result<MergedMetadata, MetadataWriteOutcome> {
        let persisted = self
            .read_persisted_record(&existing.uri, &existing.resource_id, what)
            .await?;

        let mut metadata = persisted.metadata;
        enrichment.apply_to(metadata.dublin_core_mut());
        // `date_original`/`date_parsed` are projections of the merged
        // `dc.date`, re-derived here rather than carried over from the
        // persisted record — the enrichment may have just replaced (or
        // retracted) that date, and the two columns are what the `document`
        // date axis filters on.
        let date_original = metadata.dublin_core().date.clone();
        let date_parsed = date_original
            .as_deref()
            .and_then(crate::dates::parse_partial_iso8601);

        let metadata_hash = crate::ids::compute_metadata_hash(
            &metadata,
            external_id,
            existing.external_etag.as_deref(),
            modified_at,
        );

        Ok(MergedMetadata {
            metadata,
            date_original,
            date_parsed,
            metadata_hash,
        })
    }

    /// The write tail both metadata-refresh hooks share: persist the record,
    /// and on success cache the `DocumentRecord` the hook derived for it.
    ///
    /// The two hooks share this and the read above, and nothing between them:
    /// each keeps its own derivation and its own unchanged-condition, because
    /// those genuinely differ — one folds a validator pair and compares it
    /// directly, the other merges a connector's claim and compares a metadata
    /// hash. Folding those together would need a flag that selects behavior,
    /// which is worse than the two explicit hooks. `what` is a label for the
    /// warning and selects nothing.
    ///
    /// A failed write leaves `doc_index` untouched on purpose, exactly like
    /// `on_resource`'s metadata-only branch: the stale cached hash is what
    /// makes the next run retry the write.
    async fn persist_metadata_write(
        &mut self,
        uri: &str,
        resource_id: &str,
        record: &crate::store::ResourceRecord,
        updated: DocumentRecord,
        what: &str,
    ) -> MetadataWriteOutcome {
        if let Err(e) = self
            .store
            .update_resource_metadata(&self.config.store_id, resource_id, record)
            .await
        {
            let msg = format!("error persisting refreshed {what} for '{uri}': {e}");
            tracing::warn!("{msg}");
            return MetadataWriteOutcome::Failed(msg);
        }
        self.doc_index.upsert(updated);
        MetadataWriteOutcome::Written
    }

    /// Whether `self.source` is a network source (`url`/`feed`) — the only
    /// kinds `last_checked_at` throttles a re-check against. `path` sources
    /// mtime-skip in `FileIngestor` itself; there is no origin round trip
    /// there for a check clock to mean anything about.
    fn is_network_source(&self) -> bool {
        matches!(
            self.source.spec,
            SourceSpec::Url { .. } | SourceSpec::Feed { .. }
        )
    }

    /// Advance `last_checked_at` for `resource_id` — a `200` or `304` that
    /// just left the store consistent for it — and, on success, mirror the
    /// new timestamp onto `doc_index`'s in-memory record for `uri`, if one
    /// exists, so a later gate check within the same run also sees it.
    ///
    /// `store.touch_resource_checked` computes its own `now_rfc3339()`
    /// server-side; the in-memory stamp below is a second, independent call.
    /// The two can disagree by a fraction of a second — harmless, since
    /// nothing compares them against each other, only each against a floor
    /// measured in hours.
    ///
    /// A store error (`Error::ResourceNotFound` from a concurrent delete
    /// racing this call, most commonly) is logged at debug and swallowed —
    /// mirroring how the feed liveness sweep treats a
    /// `touch_resource_liveness` failure for one candidate (`liveness.rs`):
    /// a vanished resource has nothing left for the clock to throttle, and
    /// this never counts as an error.
    ///
    /// Callers gate on [`Self::is_network_source`] first. Deliberately
    /// **not** called for `IndexOutcome::Empty`, an index-write error, a
    /// metadata-write error, `SkipReason::Error`/`Unsupported`/`Other`/
    /// `Fresh`, or `on_gone`: every one of those arms leaves `doc_index`
    /// stale on purpose so the next run retries the same URI, and stamping
    /// this clock there would let the recheck gate suppress exactly that
    /// retry (specs/04-search-pipeline.md §1 "Recheck gate").
    async fn touch_checked(&mut self, uri: &str, resource_id: &str) {
        if let Err(e) = self
            .store
            .touch_resource_checked(&self.config.store_id, resource_id)
            .await
        {
            tracing::debug!(
                uri = %uri,
                resource_id = %resource_id,
                error = %e,
                "touch_resource_checked failed, leaving last_checked_at untouched"
            );
            return;
        }
        if let Some(existing) = self.doc_index.get(uri) {
            let mut updated = existing.clone();
            updated.last_checked_at = Some(now_rfc3339());
            self.doc_index.upsert(updated);
        }
    }

    fn emit(&self, event: crate::progress::ProgressEvent) {
        if let Some(sink) = &self.progress {
            sink(event);
        }
    }

    fn start_document(&mut self, uri: &str) {
        let index = self.next_index;
        self.next_index += 1;
        self.emit(crate::progress::ProgressEvent::DocumentStarted {
            uri: uri.to_string(),
            index,
            total: self.discovered_total,
        });
    }

    /// Shared body of `on_resource` and `on_resource_fallback`. Indexing,
    /// `doc_index` updates, and counters are identical either way —
    /// `fetched_from_origin` only gates whether a real 200/304/rewrite is
    /// allowed to advance the check clock. `false` means the resource came
    /// from the feed connector's embedded-content fallback, which never
    /// contacted the entry's own origin: `is_network_source()` alone cannot
    /// tell the two apart, since a fallback resource is produced under the
    /// same `Feed` source spec as a real fetch.
    async fn on_resource_impl(
        &mut self,
        resource: Resource,
        fetched_from_origin: bool,
    ) -> Result<(), Error> {
        let uri = resource.uri.as_str().to_string();
        self.seen.insert(uri.clone());
        self.result.docs_seen += 1;
        self.start_document(&uri);

        // Skip-check: unchanged content_hash + same policy_version → either
        // an unchanged-metadata skip or a metadata-only update, decided by
        // `metadata_hash` (issue #176). Ingestors may ALSO skip earlier via
        // `on_skipped`; every path here marks the URI seen so the
        // delete-sweep leaves it alone.
        if let Some(existing) = self.doc_index.get(&uri) {
            if existing.content_hash == resource.content_hash
                && existing.policy_version == self.config.policy_version
            {
                // Computed once here — the sole use of `derive_resource_state`
                // on this branch, since neither arm below calls
                // `index_resource` (which would otherwise duplicate it).
                let derived = derive_resource_state(&resource);

                // `external_last_modified` is compared on its own because
                // it is deliberately not one of `compute_metadata_hash`'s
                // inputs (specs/02-domain-model.md §2), so a hash comparison
                // alone cannot see it move. A `Last-Modified`-only origin
                // rotates exactly that field on an unchanged 200: without
                // this the skip returns before the write, the stored
                // validator stays at whatever the first run captured, and
                // every run after replays an `If-Modified-Since` the origin
                // has already moved past — a full re-download, every run,
                // forever. The write is the metadata-only branch below,
                // which already persists the field. Same reasoning
                // `on_validators_refreshed` gives for comparing the
                // validator pair rather than the hash.
                if existing.metadata_hash == derived.metadata_hash
                    && existing.external_last_modified == resource.external_last_modified
                {
                    let resource_id = existing.resource_id.clone();
                    self.result.docs_skipped += 1;
                    // A real 200/304 reached the origin for this URI and left
                    // the store consistent for it — advance the check clock.
                    // `path` sources never reach here through a network round
                    // trip, so the gate excludes them; a fallback resource
                    // never reached the origin either, so `fetched_from_origin`
                    // excludes it too.
                    if self.is_network_source() && fetched_from_origin {
                        self.touch_checked(&uri, &resource_id).await;
                    }
                    self.emit(crate::progress::ProgressEvent::DocumentFinished {
                        uri,
                        outcome: crate::progress::DocOutcome::Skipped,
                    });
                    return Ok(());
                }

                // Content and policy are unchanged, but persisted metadata
                // differs: rewrite the resource row in place, no
                // chunks/blocks/embeddings touched.
                let resource_id = existing.resource_id.clone();
                let record = crate::store::ResourceRecord {
                    metadata: derived.metadata,
                    external_id: resource.external_id.clone(),
                    external_etag: resource.external_etag.clone(),
                    external_last_modified: resource.external_last_modified.clone(),
                    modified_at: derived.modified_at,
                    date_original: derived.date_original,
                    date_parsed: derived.date_parsed,
                };
                // Per-resource errors never abort the run (specs/04 §2),
                // mirroring the full-reindex error arm below: a metadata-only
                // write failure counts as an error and processing continues.
                // doc_index is deliberately left untouched — the stale hash
                // makes this resource retry the metadata write on the next
                // run, exactly like a failed full reindex retries.
                if let Err(e) = self
                    .store
                    .update_resource_metadata(&self.config.store_id, &resource_id, &record)
                    .await
                {
                    tracing::warn!("error updating metadata for resource '{}': {}", uri, e);
                    self.result.error_count += 1;
                    self.emit(crate::progress::ProgressEvent::DocumentFinished {
                        uri,
                        outcome: crate::progress::DocOutcome::Error,
                    });
                    return Ok(());
                }
                self.doc_index.upsert(DocumentRecord {
                    uri: uri.clone(),
                    resource_id: resource_id.clone(),
                    source_id: existing.source_id.clone(),
                    content_hash: existing.content_hash.clone(),
                    policy_version: existing.policy_version.clone(),
                    metadata_hash: derived.metadata_hash,
                    external_etag: resource.external_etag.clone(),
                    external_last_modified: resource.external_last_modified.clone(),
                    // Carries the row's existing check clock forward;
                    // `touch_checked` below then replaces it with the fresh
                    // value for a network source, exactly as the Written
                    // arm's own upsert-then-touch does.
                    last_checked_at: existing.last_checked_at.clone(),
                });
                self.result.docs_metadata_updated += 1;
                // The write above just persisted a 200/304's rewritten
                // metadata — a successful origin contact — so advance the
                // check clock the same way the unchanged-skip arm above
                // does. Gated on `fetched_from_origin` too: a fallback
                // rewrite reached no origin, so the prior check clock (just
                // carried forward into the upsert above) must survive
                // untouched.
                if self.is_network_source() && fetched_from_origin {
                    self.touch_checked(&uri, &resource_id).await;
                }
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::MetadataUpdated,
                });
                return Ok(());
            }
        }

        let replaces = self.doc_index.get(&uri).map(|e| e.resource_id.clone());

        let deps = IndexResourceDeps {
            store: self.store,
            embedder: self.embedder,
            config: self.config,
        };

        match index_resource(&resource, self.source, replaces.as_deref(), &deps).await {
            Ok(IndexOutcome::Empty) => {
                // #185: the resource chunked to nothing, so `index_resource`
                // wrote nothing and deleted nothing. Count it as a skip, not
                // as an indexed document.
                //
                // `doc_index` is deliberately left UNTOUCHED. Upserting the
                // empty resource's id/hash here would point the index at a
                // resource_id the store has no rows for (the store still holds
                // the *old* resource), which would make the next run's
                // skip-check compare against a phantom and leave the real rows
                // unreachable. The URI is already in `seen` (inserted at the
                // top of this method), so it survives the delete-sweep.
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::Skipped,
                });
            }
            Ok(IndexOutcome::Written(chunks_written, metadata_hash)) => {
                self.result.docs_indexed += 1;
                self.result.chunks_written += chunks_written as u64;
                self.doc_index.upsert(DocumentRecord {
                    uri: uri.clone(),
                    resource_id: resource.id.clone(),
                    source_id: resource.source_id.clone(),
                    content_hash: resource.content_hash.clone(),
                    policy_version: self.config.policy_version.clone(),
                    metadata_hash,
                    external_etag: resource.external_etag.clone(),
                    external_last_modified: resource.external_last_modified.clone(),
                    // A content change lands under a fresh `resource.id`
                    // (specs/02-domain-model.md §2); this upsert starts it at
                    // `None` and `touch_checked` below immediately stamps it
                    // for a network source — neither the INSERT list nor the
                    // ON CONFLICT clause of `upsert_chunks_and_blocks` writes
                    // this column, so without the touch the new row stays
                    // NULL forever.
                    last_checked_at: None,
                });
                // A content change is still a successful origin contact —
                // advance the check clock the same way an unchanged 200/304
                // does above. A fallback-produced resource never contacted
                // the origin, so it starts (and, absent a later real fetch,
                // stays) at `None`.
                if self.is_network_source() && fetched_from_origin {
                    self.touch_checked(&uri, &resource.id).await;
                }
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::Indexed {
                        chunks: chunks_written,
                    },
                });
            }
            Err(e) => {
                // Per-resource errors never abort the run (specs/04 §2).
                // doc_index is deliberately left untouched so a later run
                // retries.
                tracing::warn!("error indexing resource '{}': {}", uri, e);
                self.result.error_count += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri,
                    outcome: crate::progress::DocOutcome::Error,
                });
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl IngestCallback for PipelineCallback<'_> {
    async fn on_resource(&mut self, resource: Resource) -> Result<(), Error> {
        self.on_resource_impl(resource, true).await
    }

    /// See `on_resource_impl`'s doc comment — this is the fallback path,
    /// `fetched_from_origin: false`.
    async fn on_resource_fallback(&mut self, resource: Resource) -> Result<(), Error> {
        self.on_resource_impl(resource, false).await
    }

    async fn on_discovered(&mut self, total: usize) {
        self.discovered_total = total;
        self.emit(crate::progress::ProgressEvent::Discovered { total });
    }

    async fn on_gone(&mut self, uri: &Uri) {
        // Positively confirmed absent at the origin. Recorded rather than
        // deleted here so that all deletion happens in one place in
        // `run_source_ingestion` — but unlike the sweep's inferred deletions,
        // this one is exempt from every guard: the ingestor didn't fail to see
        // it, the origin told us it's gone.
        //
        // Deliberately NOT added to `seen`: `seen` means "still alive, don't
        // sweep", which is the opposite of what this signal says.
        self.gone.insert(uri.as_str().to_string());
    }

    async fn on_skipped(&mut self, uri: &Uri, reason: SkipReason) {
        // `uri` is already canonical by construction (see `Ingestor::on_skipped`'s
        // doc comment) — no normalization step belongs here.
        let uri = uri.as_str();
        self.seen.insert(uri.to_string());
        self.result.docs_seen += 1;
        self.start_document(uri);

        match reason {
            SkipReason::Unchanged => {
                // Still alive, just unchanged — never re-index, never sweep.
                self.result.docs_skipped += 1;
                // A real 304, or an unchanged 200, for a network source: the
                // origin was successfully contacted. Only when this run's
                // own `doc_index` actually has an entry for the URI — a
                // liveness-sweep-style probe of a URI this run never loaded
                // has nothing here to stamp.
                if self.is_network_source() {
                    if let Some(resource_id) =
                        self.doc_index.get(uri).map(|r| r.resource_id.clone())
                    {
                        self.touch_checked(uri, &resource_id).await;
                    }
                }
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Skipped,
                });
            }
            SkipReason::Unsupported => {
                // An unsupported file is counted but never deleted — it stays
                // "seen" so any previously-indexed
                // content for it (from before it became unsupported) survives
                // the sweep untouched, neither refreshed nor removed.
                self.result.unsupported_format_count += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Unsupported,
                });
            }
            SkipReason::Other(_) => {
                // No direct old-path analog; nearest classification is a
                // (non-format, non-error) skip. Alive either way (marked seen
                // above), so it survives the sweep regardless.
                self.result.docs_skipped += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Skipped,
                });
            }
            SkipReason::MetadataUpdated => {
                // Not a skip: the resource row was rewritten in place. Mirrors
                // `on_resource`'s metadata-only branch exactly — same counter,
                // same progress outcome — so a metadata write reads the same
                // whether it arrived with a body or behind a 304.
                self.result.docs_metadata_updated += 1;
                // Same origin-contact reasoning as the `Unchanged` arm above.
                if self.is_network_source() {
                    if let Some(resource_id) =
                        self.doc_index.get(uri).map(|r| r.resource_id.clone())
                    {
                        self.touch_checked(uri, &resource_id).await;
                    }
                }
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::MetadataUpdated,
                });
            }
            SkipReason::Fresh => {
                // The recheck gate found this entry fresh and made no HTTP
                // request at all — never a touch (see `touch_checked`'s doc
                // comment): this is the arm that rationale most directly
                // protects, since advancing the clock here would let the
                // gate suppress its own next check forever. Still alive
                // (marked seen above), so it survives the delete-sweep like
                // every other skip reason.
                self.result.docs_skipped += 1;
                self.result.docs_recheck_deferred += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Skipped,
                });
            }
            SkipReason::Error(ref msg) => {
                // C7/C8: processing failed but the item still exists — count
                // it as an error (not a benign skip) so the CLI summary and
                // IngestionResult.error_count reflect it accurately. Still
                // marked "seen" above, so it keeps its URI alive across the
                // delete-sweep exactly like Unchanged/Other/Unsupported do —
                // a transient failure must never look like the resource is
                // gone.
                tracing::warn!("error processing '{}': {}", uri, msg);
                self.result.error_count += 1;
                self.skip_error_count += 1;
                self.emit(crate::progress::ProgressEvent::DocumentFinished {
                    uri: uri.to_string(),
                    outcome: crate::progress::DocOutcome::Error,
                });
            }
        }
    }

    /// The suppression rule (normative) — specs/04-search-pipeline.md §1.
    ///
    /// Conditional headers are sent only when the stored resource's
    /// `policy_version` equals this run's. A 304 returns no bytes, so a
    /// resource that needs re-chunking under a changed policy could never be
    /// re-chunked if it were allowed to answer 304 — the document would be
    /// silently frozen at the old policy forever. This reuses the exact
    /// signal `on_resource`'s own skip-check gates on a few dozen lines
    /// above (`existing.policy_version == self.config.policy_version`), so
    /// the two checks can never drift apart into disagreeing about whether a
    /// resource is "current."
    ///
    /// **This is the designated join point.** Any future axis able to force
    /// reprocessing without a content change (a real `extractor_version`
    /// bump, a `Resource.mime` reclassification) must gate its own replay
    /// suppression through this same check — bypassing it reintroduces
    /// exactly the bug this comment describes, just under a different name.
    async fn lookup_fetch_metadata(&mut self, uri: &Uri) -> FetchMetadata {
        let Some(existing) = self.doc_index.get(uri.as_str()) else {
            return FetchMetadata::default();
        };
        if existing.policy_version != self.config.policy_version {
            return FetchMetadata::default();
        }
        FetchMetadata {
            etag: existing.external_etag.clone(),
            last_modified: existing.external_last_modified.clone(),
        }
    }

    async fn on_validators_refreshed(
        &mut self,
        uri: &Uri,
        meta: &FetchMetadata,
    ) -> MetadataWriteOutcome {
        // A bare 304 (both `None`) means "keep whatever is already stored"
        // — never "clear it" (see `FetchResult::NotModified`'s doc comment).
        // Nothing to persist.
        if meta.etag.is_none() && meta.last_modified.is_none() {
            return MetadataWriteOutcome::Unchanged;
        }
        let uri_str = uri.as_str().to_string();
        let Some(existing) = self.doc_index.get(&uri_str).cloned() else {
            // Nothing indexed for this URI under the current doc_index (a
            // concurrent delete raced this fetch, or the liveness sweep is
            // probing a URI this run's doc_index never loaded) — nothing to
            // refresh.
            return MetadataWriteOutcome::Unchanged;
        };

        let new_etag = meta.etag.clone().or_else(|| existing.external_etag.clone());
        let new_last_modified = meta
            .last_modified
            .clone()
            .or_else(|| existing.external_last_modified.clone());

        // An origin is free to repeat the validator it already issued, and a
        // well-behaved one does exactly that on every 304 for unchanged
        // content — so this is the common case, not an edge. Writing anyway
        // would rewrite the resource row and bump `index_updated_at` (public
        // as `DocumentInfo.index_updated_at`) on a run that changed nothing.
        //
        // The comparison is on the validator pair itself, not on
        // `compute_metadata_hash` as `on_metadata_refreshed` below uses:
        // `external_last_modified` is deliberately not one of that hash's
        // inputs (specs/02-domain-model.md §2), so a 304 rotating only
        // `Last-Modified` produces an identical hash while still needing to
        // be persisted. A hash guard here would silently drop it.
        if new_etag == existing.external_etag
            && new_last_modified == existing.external_last_modified
        {
            return MetadataWriteOutcome::Unchanged;
        }

        // RFC 9111 requires storing whichever validator(s) the 304 itself
        // carried, but content and the resource's own metadata are
        // unchanged by definition (a 304 has no body) — this never triggers
        // a re-chunk or re-embed, and it never goes through `index_resource`
        // or `on_resource`'s own metadata-only branch (neither has a
        // `Resource` to work from here, only a `Uri` and a `FetchMetadata`).
        //
        // The subtle part: `external_etag` IS an input to
        // `compute_metadata_hash`, but `external_last_modified` deliberately
        // is not (specs/02-domain-model.md §2). `resources.external_etag` is
        // about to change, and `list_indexed_documents` recomputes
        // `metadata_hash` straight from that same column on every
        // rehydration — so leaving the *cached* `metadata_hash` in
        // `doc_index` unrefreshed would desync it from what a fresh
        // rehydration computes for the same row. The next metadata-unchanged
        // fetch for this URI would then see a spurious `metadata_hash`
        // mismatch and route through a needless metadata-only update purely
        // to correct a staleness this method introduced. So this recomputes
        // and re-caches `metadata_hash` in lockstep with the rotated
        // validator, over the resource's current persisted state —
        // `update_resource_metadata` rewrites every column of that state, so
        // every column it does not mean to change has to be read back first,
        // and `DocumentRecord` carries only the hash, not what was hashed.
        let resource_id = existing.resource_id.clone();
        let persisted = match self
            .read_persisted_record(&uri_str, &resource_id, VALIDATORS)
            .await
        {
            Ok(record) => record,
            Err(outcome) => return outcome,
        };

        let record = crate::store::ResourceRecord {
            external_etag: new_etag.clone(),
            external_last_modified: new_last_modified.clone(),
            ..persisted
        };
        let metadata_hash = crate::ids::compute_metadata_hash(
            &record.metadata,
            record.external_id.as_deref(),
            new_etag.as_deref(),
            record.modified_at.as_deref(),
        );

        self.persist_metadata_write(
            &uri_str,
            &resource_id,
            &record,
            DocumentRecord {
                metadata_hash,
                external_etag: new_etag,
                external_last_modified: new_last_modified,
                ..existing
            },
            VALIDATORS,
        )
        .await
    }

    /// A 304 carries no body, so the connector's own metadata for the
    /// resource — which it re-supplies on every run, independently of the
    /// fetch — is the only thing that can have changed. Layer it back onto
    /// the persisted state and write only if the result actually differs.
    ///
    /// The comparison is the point, not an optimization: an unchanged feed
    /// entry is the overwhelmingly common case, and a blind write would turn
    /// every 304 into a resource-row rewrite, bumping `index_updated_at`
    /// (publicly visible as `DocumentInfo.index_updated_at`) on a run that
    /// changed nothing. So this recomputes `metadata_hash` from the merged
    /// state and returns early when it matches what is already cached —
    /// exactly the equality the skip-check in `on_resource` performs, on the
    /// same derivation, for the same reason.
    ///
    /// The merge runs against the *persisted* metadata rather than a fresh
    /// parse, because there is no fresh parse to run. One consequence
    /// follows from `MetadataEnrichment`'s title rule and is intended: a
    /// connector title only fills a gap, so a feed that renames an entry
    /// whose linked page supplied its own title changes nothing here — which
    /// is what a full re-fetch would conclude too, since the page's title
    /// would win again. Where the two paths do differ is the rarer case of a
    /// page with no title of its own: the persisted title is then the
    /// connector's previous one, no longer a gap, so a renamed entry keeps
    /// the old title until its content changes. Erring toward keeping
    /// extracted state is the safe direction; the overwrite-class fields
    /// (`creator`, `date`, provenance, `external_id`, `modified_at`), where
    /// staleness actually costs something, take the connector's current claim
    /// — including the absence of one. A connector that stops claiming a
    /// `date` it previously stamped retracts it (`MetadataEnrichment::
    /// apply_to`), and `external_id`/`modified_at` are authoritative as
    /// passed, `None` included. A connector that stops claiming a `creator`
    /// is the one case that does not retract: `creator` carries no
    /// provenance stamp, so there is no way to tell the connector's own
    /// previous value from the extraction's.
    async fn on_metadata_refreshed(
        &mut self,
        uri: &Uri,
        enrichment: &crate::metadata::MetadataEnrichment,
        external_id: Option<&str>,
        modified_at: Option<&str>,
    ) -> MetadataWriteOutcome {
        let uri_str = uri.as_str().to_string();
        let Some(existing) = self.doc_index.get(&uri_str).cloned() else {
            // Nothing indexed for this URI under the current doc_index —
            // same race `on_validators_refreshed` tolerates.
            return MetadataWriteOutcome::Unchanged;
        };

        let resource_id = existing.resource_id.clone();
        let merged = match self
            .merge_metadata_claim(
                &existing,
                enrichment,
                external_id,
                modified_at,
                CONNECTOR_METADATA,
            )
            .await
        {
            Ok(merged) => merged,
            Err(outcome) => return outcome,
        };
        if merged.metadata_hash == existing.metadata_hash {
            return MetadataWriteOutcome::Unchanged;
        }

        let record = crate::store::ResourceRecord {
            metadata: merged.metadata,
            external_id: external_id.map(str::to_string),
            external_etag: existing.external_etag.clone(),
            external_last_modified: existing.external_last_modified.clone(),
            modified_at: modified_at.map(str::to_string),
            date_original: merged.date_original,
            date_parsed: merged.date_parsed,
        };

        self.persist_metadata_write(
            &uri_str,
            &resource_id,
            &record,
            DocumentRecord {
                metadata_hash: merged.metadata_hash,
                ..existing
            },
            CONNECTOR_METADATA,
        )
        .await
    }

    /// The recheck gate (specs/04-search-pipeline.md §1 "Recheck gate"):
    /// asked by the feed ingestor for a fetchable discovery entry before any
    /// HTTP request is made for it. Three conditions, checked cheapest-first
    /// with the store read last, must ALL hold for this to return `false`
    /// ("report it fresh, skip the fetch"); any one failing falls through to
    /// `true` ("go fetch"), exactly the conditional GET that ran before this
    /// gate existed.
    ///
    /// - **(a) known and current.** `uri` is in `doc_index` and its stored
    ///   `policy_version` equals this run's — the exact predicate
    ///   [`Self::lookup_fetch_metadata`]'s own suppression rule gates
    ///   conditional headers on, so a `policy_version` bump reopens both
    ///   checks together rather than one at a time.
    /// - **(c) inside the recheck floor.** `existing.last_checked_at` is
    ///   `Some` and lies within `recheck::recheck_floor_start_rfc3339`'s
    ///   floor of now. Both `last_checked_at` and the floor's own start are
    ///   produced by `to_rfc3339_opts(SecondsFormat::Secs, true)` — RFC 3339,
    ///   second precision, explicit `Z`/offset — which sorts
    ///   lexicographically identically to chronological order, so the plain
    ///   string comparison below is a valid `>=` test without parsing either
    ///   side back into a `DateTime`. `self.refetch` (`--refetch`) bypasses
    ///   this check unconditionally, the same escape hatch that also
    ///   suppresses the feed document's own validators one layer up
    ///   (`run_source_ingestion`).
    /// - **(b) claim reproduces the stored hash.** The feed's current claim
    ///   — `enrichment`/`external_id`/`modified_at` — merged over the
    ///   persisted record via [`Self::merge_metadata_claim`] (the same
    ///   read-only merge [`Self::on_metadata_refreshed`] applies when it
    ///   writes) reproduces `existing.metadata_hash` exactly. A store miss
    ///   or a store read error opens the gate — falling through to a fetch
    ///   is always safe, so an ambiguous read must never be read as "fresh".
    async fn recheck_is_due(
        &mut self,
        uri: &Uri,
        enrichment: &crate::metadata::MetadataEnrichment,
        external_id: Option<&str>,
        modified_at: Option<&str>,
    ) -> bool {
        let uri_str = uri.as_str();

        // (a) known and current — no store read needed either way.
        let Some(existing) = self.doc_index.get(uri_str).cloned() else {
            return true;
        };
        if existing.policy_version != self.config.policy_version {
            return true;
        }

        // (c) inside the floor — still no store read.
        if self.refetch {
            return true;
        }
        let Some(last_checked_at) = existing.last_checked_at.as_deref() else {
            return true;
        };
        let refresh_interval_secs = match &self.source.spec {
            SourceSpec::Feed {
                refresh_interval_secs,
                ..
            } => *refresh_interval_secs,
            _ => None,
        };
        let floor = crate::ingestion::recheck::recheck_floor_start_rfc3339(
            chrono::Utc::now(),
            refresh_interval_secs,
        );
        if last_checked_at < floor.as_str() {
            return true;
        }

        // (b) claim reproduces the stored hash — the store read, last
        // because it is the only one of the three that isn't free.
        match self
            .merge_metadata_claim(
                &existing,
                enrichment,
                external_id,
                modified_at,
                RECHECK_CLAIM,
            )
            .await
        {
            Ok(merged) => merged.metadata_hash != existing.metadata_hash,
            Err(_) => true,
        }
    }

    /// See `IngestCallback::due_entries_for_source`'s doc comment
    /// (specs/04-search-pipeline.md §1 "Due-entry revisit on a feed 304").
    /// Meaningful only for `SourceSpec::Feed` — every other source kind has
    /// no feed-304 concept, so it returns nothing for them.
    async fn due_entries_for_source(&mut self) -> Vec<DueRecheckEntry> {
        let SourceSpec::Feed {
            url: feed_url,
            refresh_interval_secs,
            ..
        } = &self.source.spec
        else {
            return Vec::new();
        };

        // Same floor derivation `recheck_is_due`'s check (c) uses — the two
        // mechanisms must never drift apart on what "inside the floor" means
        // for the same column.
        let floor = crate::ingestion::recheck::recheck_floor_start_rfc3339(
            chrono::Utc::now(),
            *refresh_interval_secs,
        );

        // A link-less entry is stored under the synthetic `{feed_url}#entry:`
        // shape (`synthetic_entry_uri` in `ingest/src/feed_ingestor.rs`) —
        // there is nothing to fetch at that locator, so it must never become
        // a revisit candidate. `core` cannot call that function directly (it
        // lives one layer up, in `ingest`), so this mirrors its exact prefix;
        // the two must be kept in sync if that shape ever changes.
        let synthetic_prefix = format!("{feed_url}#entry:");

        let mut candidates: Vec<DocumentRecord> = self
            .doc_index
            .for_source(&self.source.id)
            .filter(|r| !r.uri.starts_with(&synthetic_prefix))
            .filter(|r| {
                r.policy_version != self.config.policy_version
                    || r.last_checked_at
                        .as_deref()
                        .is_none_or(|checked| checked < floor.as_str())
            })
            .cloned()
            .collect();

        // Oldest-first, `None` leading — the same ordering the liveness
        // sweep's own candidate query uses, and for the same reason: a
        // long-idle source drains its backlog across successive runs instead
        // of being capped forever by whichever candidates happen to sort
        // first.
        candidates.sort_by(|a, b| a.last_checked_at.cmp(&b.last_checked_at));
        // Same batch bound the liveness sweep uses
        // (specs/04-search-pipeline.md §1 "Aged-out feed entries: the
        // liveness sweep" → "Candidates") — one shared cap keeps a quiet
        // feed's backlog bounded the same way on both paths.
        candidates.truncate(FEED_LIVENESS_BATCH_LIMIT);

        let mut out = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            // A store miss (a concurrent delete racing this revisit) or a
            // read error skips the candidate silently — there is nothing to
            // rebuild a claim from, and the next run's candidate query will
            // simply no longer see it once the delete lands in `doc_index`.
            let record = match self
                .store
                .get_resource_record(&self.config.store_id, &candidate.resource_id)
                .await
            {
                Ok(Some(record)) => record,
                Ok(None) => {
                    tracing::debug!(
                        uri = %candidate.uri,
                        "due-entry revisit: resource record missing, likely a concurrent delete; skipping candidate"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::debug!(
                        uri = %candidate.uri,
                        error = %e,
                        "due-entry revisit: error reading resource record; skipping candidate"
                    );
                    continue;
                }
            };
            let Some(uri) = Uri::parse(&candidate.uri) else {
                tracing::debug!(
                    uri = %candidate.uri,
                    "due-entry revisit: stored URI failed to reparse; skipping candidate"
                );
                continue;
            };

            // Rebuild the connector's claim from the resource's own
            // persisted Dublin Core fields. `MetadataEnrichment::apply_to` is
            // idempotent (see its doc comment), so feeding this back through
            // the identical merge the ordinary entry loop uses reproduces
            // exactly the stored state — "revisit with this claim" and "keep
            // what is already stored" are the same operation here.
            let dc = record.metadata.dublin_core();
            let enrichment = crate::metadata::MetadataEnrichment {
                title_fallback: dc.title.clone(),
                creator: dc.creator.clone(),
                date: dc.date.clone(),
                date_source: dc.date_source.clone(),
                provenance_source: dc.source.clone(),
            };

            out.push(DueRecheckEntry {
                locator: candidate.uri.clone(),
                uri,
                enrichment,
                external_id: record.external_id.clone(),
                modified_at: record.modified_at.clone(),
            });
        }
        out
    }
}
