//! The feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out feed
//! entries: the liveness sweep"): for a `SourceSpec::Feed` source, probes a
//! bounded batch of feed-discovered resources this run did not observe
//! against their stored link, and deletes only the ones a probe *positively
//! confirms* gone (404/410).
//!
//! "Did not observe" is the candidate rule; "aged out of the window" is the
//! case it exists to serve, and the two are not the same. Nothing persists
//! window membership, and a run whose feed document answered 304 observed
//! nothing at all — so an entry the window still lists is a candidate on
//! such a run. See the spec section named above; the bound is the delete
//! rule, not the candidate rule.
//!
//! This sits in the confirmed-gone bucket alongside `IngestCallback::on_gone`
//! (handled directly in `super::run_source_ingestion`), not the
//! presumed-gone delete-sweep also in `super`: it never deletes on absence
//! alone, only on the origin's own answer. `pub(in crate::ingestion)` on the
//! two entry points below rather than a wider visibility: only
//! `run_source_ingestion` calls into this module, and nothing outside
//! `ingestion` needs to.

use chrono::{SecondsFormat, Utc};

use crate::error::Error;
use crate::ingestion::deps::{DocumentIndex, FetchMetadata, FetchResult, UrlFetcher};
use crate::ingestion::{IngestionConfig, IngestionResult};
use crate::store::{RetrievalStore, StaleFeedResource};
use crate::types::Source;

/// The local-inputs digest for `source` under `config`, or `None` when the
/// source has no feed document whose fetch could be made conditional.
///
/// The `None` return is what keeps every non-feed source out of the gate
/// entirely: with no digest there is nothing to compare, nothing to
/// suppress, and nothing to persist.
pub(in crate::ingestion) fn feed_inputs_digest(
    source: &Source,
    config: &IngestionConfig,
) -> Option<String> {
    match &source.spec {
        crate::types::SourceSpec::Feed {
            max_entries,
            fetch_full_content,
            ..
        } => Some(crate::ids::compute_feed_inputs_digest(
            &config.policy_version,
            *fetch_full_content,
            *max_entries,
        )),
        _ => None,
    }
}

/// Batch cap for the feed liveness sweep: at most this many aged-out feed
/// entries are probed per source per run. See
/// specs/04-search-pipeline.md §1 "Aged-out feed entries: the liveness
/// sweep". `pub(in crate::ingestion)`: exercised directly by tests in the
/// sibling `ingestion::tests` module.
pub(in crate::ingestion) const FEED_LIVENESS_BATCH_LIMIT: usize = 25;

/// Recheck floor for the feed liveness sweep, in seconds: a candidate is
/// never re-probed more often than this, however long it has been aged out.
/// A feed source's own `refresh_interval_secs` raises the effective floor
/// when configured above this; the common unconfigured case uses this bare
/// value.
const FEED_LIVENESS_MIN_RECHECK_SECS: i64 = 24 * 60 * 60;

/// Ceiling on how far the candidate query may over-fetch to compensate for
/// the seen-set it cannot see (see [`run_feed_liveness_sweep`]). Chosen well
/// above any realistic feed window, and fixed rather than derived, because
/// `max_entries` is optional and defaults to unbounded — the seen-set has no
/// principled size of its own to scale by. `pub(in crate::ingestion)`:
/// exercised directly by tests in the sibling `ingestion::tests` module.
pub(in crate::ingestion) const FEED_LIVENESS_OVERFETCH_CAP: usize = 500;

/// The feed liveness sweep (specs/04-search-pipeline.md §1 "Aged-out feed
/// entries: the liveness sweep"). For a `SourceSpec::Feed` source, probes a
/// bounded batch of feed-discovered resources this run did not observe
/// against their stored link, and deletes only the ones a probe *positively
/// confirms* gone (404/410).
///
/// This sits in the confirmed-gone bucket alongside `IngestCallback::on_gone`
/// above, not the presumed-gone one: it never deletes on absence alone, only
/// on the origin's own answer. That is what lets it coexist with the feed
/// exemption from the presumed-gone sweep above without contradicting it —
/// an entry merely scrolling off the window is still never, on its own, a
/// deletion signal; only a confirmed 404/410 on its own link is.
///
/// What the candidate rule does *not* promise is that a candidate has aged
/// out. Nothing persists window membership, so "did not observe" is all the
/// query has, and on a feed-document 304 that is every entry the source
/// owns. An entry the window still lists can therefore be probed and, on a
/// confirmed 404/410, pruned — bounded by `--delete` (the caller gates on
/// it) and by the origin's own answer, not by the candidate rule.
///
/// Callers must suppress this on an `Enumeration::Incomplete` run — a run
/// that could not read the feed's window knows nothing at all about which
/// entries it holds, so every previously indexed URI would queue for
/// probing off a signal already known to be broken. A *zero-seen* run, by contrast, must still reach this
/// function: that is the routine feed-304 case, and suppressing it starves
/// the sweep exactly when a feed goes quiet (specs/04-search-pipeline.md §1
/// "Guards"). See the call site in `run_source_ingestion`. This function
/// performs no guard check of its own; it probes whatever
/// [`RetrievalStore::list_stale_feed_resources`] returns, minus `seen`, up to
/// [`FEED_LIVENESS_BATCH_LIMIT`].
///
/// The caller builds the [`LivenessProbeContext`] rather than handing over
/// its five parts for this function to bundle: the bundle exists either way,
/// since every candidate probe needs it, and building it one frame earlier
/// is what keeps this signature to the four values that are actually this
/// sweep's own — which source, how often, and what it already saw.
pub(in crate::ingestion) async fn run_feed_liveness_sweep(
    ctx: &mut LivenessProbeContext<'_>,
    source_id: &str,
    refresh_interval_secs: Option<u64>,
    seen: &std::collections::HashSet<String>,
) -> Result<(), Error> {
    let floor_secs = refresh_interval_secs
        .unwrap_or(0)
        .max(FEED_LIVENESS_MIN_RECHECK_SECS as u64);
    // `refresh_interval_secs` is an unvalidated `u64` from config (no upper
    // bound is enforced in `core::config::refresh::validate_refresh_interval`),
    // so it must not be cast with `as i64`: a value above `i64::MAX` wraps
    // negative, pushing `checked_before` into the future and making every
    // resource a candidate — the opposite of this floor's purpose. Saturate
    // every step instead of only the cast: `chrono::Duration::seconds` itself
    // panics above `i64::MAX / 1_000`, and subtracting from `Utc::now()` can
    // in principle underflow past the representable range.
    let floor_secs_i64 = i64::try_from(floor_secs).unwrap_or(i64::MAX);
    let recheck_window =
        chrono::Duration::try_seconds(floor_secs_i64).unwrap_or(chrono::Duration::MAX);
    let checked_before = Utc::now()
        .checked_sub_signed(recheck_window)
        .unwrap_or(chrono::DateTime::<Utc>::MIN_UTC)
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    // The batch cap counts candidates actually *probed*, not rows returned.
    // The query orders oldest-`last_checked_at` first and knows nothing
    // about this run's in-memory seen-set, so a run whose freshly-observed
    // entries happen to sort oldest would fill a SQL-side `LIMIT 25`
    // entirely with entries it had just seen and probe nothing at all —
    // permanently, for a feed whose whole window sorts that way. Over-fetch
    // by the seen-set's size, subtract it here, then take the real 25.
    let query_limit = FEED_LIVENESS_BATCH_LIMIT
        .saturating_add(seen.len())
        .min(FEED_LIVENESS_OVERFETCH_CAP);
    let candidates: Vec<StaleFeedResource> = ctx
        .store
        .list_stale_feed_resources(ctx.store_id, source_id, &checked_before, query_limit)
        .await?;

    for candidate in candidates
        .into_iter()
        // This run's own ingestion pass already observed this entry, so
        // probing it here would only repeat what that pass just established.
        // Reachable when a currently-live entry's `last_checked_at` happens
        // to be unset or stale — it has simply never been probed, or was
        // probed long ago while still current. Note this is all the sweep
        // knows about window membership: on a run that observed nothing,
        // this filter subtracts nothing.
        .filter(|candidate| !seen.contains(&candidate.uri))
        .take(FEED_LIVENESS_BATCH_LIMIT)
    {
        probe_liveness_candidate(ctx, candidate).await?;
    }

    Ok(())
}

/// Everything the sweep and each candidate probe need beyond the candidate
/// itself: the store to query and write through, the fetcher to probe with,
/// the index to keep in step, and the run's result to record into.
///
/// One bundle for both, deliberately. Splitting the sweep in two moved this
/// parameter list rather than duplicating it, and having the caller
/// construct it — see [`run_feed_liveness_sweep`] — keeps a second,
/// structurally identical bundle from appearing beside it.
pub(in crate::ingestion) struct LivenessProbeContext<'a> {
    pub(in crate::ingestion) store_id: &'a str,
    pub(in crate::ingestion) doc_index: &'a mut DocumentIndex,
    pub(in crate::ingestion) store: &'a dyn RetrievalStore,
    pub(in crate::ingestion) fetcher: &'a dyn UrlFetcher,
    pub(in crate::ingestion) result: &'a mut IngestionResult,
}

/// Probe one aged-out feed entry and record the outcome.
///
/// Every outcome except a confirmed 404/410 converges on a single
/// `touch_resource_liveness` call; the only thing an outcome decides is
/// which validators go with it. `last_checked_at` therefore advances on
/// **every attempt** — the normative meaning of that column
/// (specs/04-search-pipeline.md §1). It has to: the candidate query is
/// oldest-first, so leaving an unreachable candidate's timestamp where it
/// was would put it back at the head of the next query, and a source with
/// `FEED_LIVENESS_BATCH_LIMIT` or more permanently-blocked entries would
/// re-probe that same stuck set forever while no other candidate ever
/// reached a batch.
///
/// `Err` is returned only for a failure that makes continuing the sweep
/// meaningless (a failed delete); per-candidate write failures are logged
/// and skipped, so one racing delete cannot discard the stats already
/// computed for the candidates processed alongside it.
async fn probe_liveness_candidate(
    ctx: &mut LivenessProbeContext<'_>,
    candidate: StaleFeedResource,
) -> Result<(), Error> {
    let stored = FetchMetadata {
        etag: candidate.external_etag.clone(),
        last_modified: candidate.external_last_modified.clone(),
    };

    // Counted for every candidate an attempt is made for, regardless of
    // outcome — see `IngestionResult::feed_entries_liveness_checked`'s
    // doc comment.
    ctx.result.feed_entries_liveness_checked += 1;

    let refreshed = match ctx.fetcher.fetch(&candidate.uri, &stored).await {
        Err(e) => {
            // A transport error is evidence of nothing about the entry, so
            // nothing about its content, metadata or validators moves — only
            // the clock, so the rotation stays fair.
            tracing::debug!(
                uri = %candidate.uri,
                error = %e,
                "feed liveness sweep: fetch error, advancing only the probe clock"
            );
            stored
        }
        Ok(FetchResult::Gone) => {
            ctx.doc_index.remove(&candidate.uri);
            let deleted = ctx.store.delete_by_resource(&candidate.resource_id).await?;
            if deleted > 0 {
                ctx.result.docs_deleted += 1;
            }
            return Ok(());
        }
        Ok(FetchResult::NotModified {
            etag,
            last_modified,
        }) => {
            // A bare 304 means unchanged; fold any rotated validator over
            // what was already stored rather than reporting a partial value
            // that would read as "clear it" — mirrors the feed document's
            // own 304 handling (`ingest::FeedIngestor::ingest`). Still
            // there, so not a delete.
            FetchMetadata {
                etag: etag.or(stored.etag),
                last_modified: last_modified.or(stored.last_modified),
            }
        }
        Ok(FetchResult::Downloaded { .. }) => {
            // The body is deliberately discarded: an aged-out entry's
            // feed-sourced metadata (title, author, per-entry date) is long
            // gone, and re-indexing from the bare page alone would silently
            // degrade the stored resource rather than improve it.
            //
            // The response's *validators* are discarded with it, and that is
            // the whole point. Storing them would leave the resource
            // pointing at a representation this store never indexed: the
            // next probe would answer 304, and if the entry ever re-entered
            // the feed window that 304 would suppress the reindex of the
            // changed content indefinitely. Replaying the stored validators
            // instead costs a full 200 at every recheck on an entry that
            // genuinely changed — pure overhead on something the sweep does
            // not re-index anyway, and the correct side to err on.
            stored
        }
        Ok(FetchResult::Blocked) => {
            // Neither evidence of anything, like a transport error — only
            // the clock moves. This arm is reachable in production:
            // `fetcher` here is the public-only client (see
            // `SourceIngestionDeps::fetcher`), and a real entry link that
            // resolves to a non-globally-routable address (an internal/LAN
            // link — specs/02-domain-model.md's "Destination policy (entry
            // links)") is refused exactly like it would be on the ordinary
            // ingestion pass. A fragment URI (a link-less entry's synthetic
            // `{feed_url}#entry:{id}`) can no longer drive this arm — it
            // never becomes a candidate in the first place
            // (`RetrievalStore::list_stale_feed_resources`).
            stored
        }
    };

    // A per-candidate failure here (e.g. a concurrent delete racing this
    // probe) must not abort the whole source and discard the run's
    // already-computed stats for the candidates already processed: log and
    // move on to the next candidate.
    if let Err(e) = ctx
        .store
        .touch_resource_liveness(
            ctx.store_id,
            &candidate.resource_id,
            refreshed.etag.as_deref(),
            refreshed.last_modified.as_deref(),
        )
        .await
    {
        tracing::debug!(
            uri = %candidate.uri,
            error = %e,
            "feed liveness sweep: touch_resource_liveness failed, leaving \
             resource untouched"
        );
    }

    Ok(())
}
