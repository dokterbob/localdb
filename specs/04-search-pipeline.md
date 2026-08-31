# Spec 04 — Ingestion & Retrieval Pipeline

> Status: accepted draft, 2026-06-30.

Pipeline: **acquire → extract → blocks → chunks → embed → index** (write side), and **query → BM25 +
dense → RRF fuse → filter → citations** (read side).

## 1. Acquisition

Acquisition is driven by **ingestors** (`Ingestor` trait in `core`). The ingestor kind determines
how content reaches the pipeline and what kind of Resource it produces.

### Ingestor kinds

- **`file` ingestor:** Scans paths on demand (`localdb index`); the daemon additionally watches
  continuously via `notify` (FSEvents on macOS, inotify on Linux), with debounce so editor
  save-storms coalesce. Include/exclude globs from the source spec. Runs the parser chain on each
  file, receives a `ParsedDocument`, and converts it to a `Resource` with typed blocks via
  `markdown_to_blocks()`. The parser chain is an implementation detail of this ingestor, not a
  top-level pipeline concept.
- **`url` ingestor:** HTTP fetch; runs the same parser chain (readability-style main-content
  extraction → Markdown); converts `ParsedDocument` to a `Resource` with blocks via
  `markdown_to_blocks()`. Conditional GET (ETag/Last-Modified) when available. Refreshes on the
  configured interval (daemon) or on explicit `localdb index` (embedded).
- **Future ingestors** (notion, telegram, signal, email, transcription, feed): produce `Resource`
  objects directly with native block types (e.g. `Message`, `Segment`, `Attachment`), bypassing the
  parser chain entirely. The `Ingestor` trait's contract is to deliver a `Resource`; how it gets
  there is ingestor-internal.

### Incremental re-index

A resource is re-processed only when its `content_hash` changes. `content_hash` is a blake3 hash of
the ordered canonical texts of all blocks in the resource (not a hash of a Markdown string).
Unchanged → skip; changed → replace-by-URI: the old resource's chunks/blocks/resource row are
deleted AND the new ones (new content-addressed IDs, [02-domain-model.md](02-domain-model.md) §3)
are inserted in a **single atomic store transaction**. A write failure during the replace (e.g. an
embedding or constraint error) leaves the old resource intact and searchable — the store never
observes a state where the old chunks are gone but the new ones failed to land.

Resources also carry:

- **`external_etag`/`external_last_modified`:** the two stored conditional-GET validators for URL
  sources and feed entry links (`resources.external_etag`, `resources.external_last_modified`).
  Whichever the origin supplied on the last successful fetch is replayed byte-exact — including
  quotes and any `W/` weak-validator prefix — as `If-None-Match`/`If-Modified-Since` on the next
  fetch of the same URI. A 304 response means the origin's own representation is unchanged: the
  resource is skipped without re-download or re-extraction.

  It says nothing, however, about metadata a **connector** supplies out of band — a feed's title,
  byline and publication date for an entry, which arrive with every read of the feed regardless of
  what that entry's link answers. That claim is re-merged onto the persisted metadata on a 304
  (`IngestCallback::on_metadata_refreshed`) under the same merge rule the index-time path applies,
  and persisted **only when the merged result actually differs** — an unchanged entry performs no
  write at all, which is what keeps a 304 cheap in the common case. Without this a feed correcting
  an entry's byline would never land for as long as the linked page itself stayed unchanged, which
  for an aging entry is indefinitely. The merge rule is deliberately asymmetric and identical on
  both paths: a connector title only fills a gap the extraction left, while byline, date and
  provenance overwrite it — a feed knows an entry's author better than the linked page's markup
  does, and does not know the page's title better than the page does. That gap-fill is not stamped,
  though, so once it has happened a title the feed supplied is indistinguishable from one the page's
  markup produced, and a feed that later corrects the title can never land the correction behind a
  304 — the same missing-provenance shape `creator` has, and ticketed alongside it
  ([#324](https://github.com/dokterbob/localdb/issues/324), pairing with
  [#320](https://github.com/dokterbob/localdb/issues/320)).

  The refreshed claim is **not** confined to `Metadata`. A connector also supplies the entry's
  `modified_at` and its `external_id`, which are stored as their own resource columns rather than as
  metadata fields, and `on_metadata_refreshed` takes both alongside the enrichment. Both are
  overwrite-class and authoritative exactly as passed — `None` included, so a connector that stops
  claiming a `modified_at` withdraws it — and both are `compute_metadata_hash` inputs, so each one
  participates in the differs-or-not comparison that gates the write. A 304 whose feed moved only an
  entry's `modified_at` is therefore a write; a 304 that brings nothing new across metadata,
  `modified_at` and `external_id` alike is not.

  A connector's `date` can also be **withdrawn**, not only overwritten: an entry that drops its
  `<pubDate>` retracts the date it previously supplied. The retraction is scoped by the
  `date_source` provenance stamp (`"feed-entry"`), so only a date this same connector wrote is ever
  taken back and an extraction-derived date is never touched. `creator` carries no such stamp and
  therefore cannot be retracted — a feed that stops naming an author keeps the last one it gave,
  since nothing distinguishes it from an author the page's own markup supplied. Giving `creator` its
  own provenance field would close that half.

  > **How a writing 304 is reported (normative).** Both refresh hooks may write, so what the run
  > reports is the _merged_ outcome of the two, reported exactly once for the URI: nothing written →
  > a plain skip (`docs_skipped`); either hook wrote → `docs_metadata_updated`, the same counter and
  > the same `DocOutcome::MetadataUpdated` an ordinary metadata-only update produces; either hook's
  > write failed → an error (`error_count`), never a clean skip. A URI is never counted in two
  > outcome buckets — those counters partition `docs_seen` — and it is marked `seen` on every
  > branch, so a 304 whose metadata write failed still survives the delete-sweep: a failed write is
  > not evidence the resource is gone.

  RFC 9111 requires storing whichever validators a 304 response itself carries, so a 304 bearing a
  refreshed `ETag`/`Last-Modified` still updates the stored columns — but only when the pair
  actually moved. A compliant origin repeats the validator it already issued on every 304 for
  unchanged content, which is the common case, and rewriting the row for it would bump the publicly
  visible `index_updated_at` on a run that changed nothing. The comparison is on the **validator
  pair itself**, deliberately not on `metadata_hash`: `external_last_modified` is not one of that
  hash's inputs, so a 304 rotating only `Last-Modified` yields an identical hash while still needing
  to be persisted.

  When the pair did move, `metadata_hash` **is** recomputed and rewritten alongside it, because
  `external_etag` is one of `compute_metadata_hash`'s own inputs ("Metadata-only update" below).
  Leaving it stale would desync the in-memory `DocumentIndex` from what a rehydration of that same
  row computes, and the next run would then read a `metadata_hash` mismatch on a resource nothing
  had changed about and take the metadata-only-update branch for no reason. No re-chunk follows
  either way — the content is unchanged by definition.

  > **Rebuilding the row (normative).** `update_resource_metadata` rewrites _every_ metadata column,
  > so a caller changing one field must read the row's current state for all the others first, via
  > `RetrievalStore::get_resource_record`. A chunk read is not a substitute and must not be used:
  > `external_id`, `date_original` and `date_parsed` are write-only on `ChunkRecord` by design, so a
  > record rebuilt from a chunk carries `None` for each and writes `NULL` over three live columns on
  > every refresh. Widening the chunk projection is the wrong repair — it also backs
  > `dense_search`/`bm25_search`, so every search row would carry and parse fields no search
  > consumer reads.

  > **Suppression rule (normative).** Conditional headers are sent **only** when the stored
  > resource's `policy_version` equals the run's. A 304 returns no bytes, so a resource that would
  > need re-chunking under a changed policy could never be re-chunked if it were allowed to
  > answer 304. This reuses the exact signal the skip-check already gates on
  > (`PipelineCallback::on_resource`). Any future axis that can force reprocessing without a content
  > change — a real `extractor_version`, a `Resource.mime` change — must join this same suppression
  > check; this is the designated join point.

  Validators are keyed to the **configured** URI, never to a redirect target — consistent with the
  feed connector's pinned-identity rule ([02-domain-model.md](02-domain-model.md), "Feed
  connector"). Usually the cost is only a missed cache hit: the redirect target's own validator
  would have matched, and instead the configured URI is refetched in full. It is not purely an
  optimization, though. A validator is scoped to the resource that issued it, and this key replays
  one issued by whatever the configured URI resolved to on the last run against whatever it resolves
  to now. If the redirect target changes and the new target happens to answer with the same opaque
  `ETag` value, the origin returns 304 about a representation our stored bytes never came from, and
  the change stays invisible until one side's validator moves. That window is accepted as remote
  rather than closed — a collision needs two distinct targets behind one URI to pick the same
  validator string — but it is a staleness risk, not merely a lost cache hit.

- **`extractor_version`:** a version stamp on the parser/block-conversion logic. When parser or
  `markdown_to_blocks()` logic improves, bumping `extractor_version` would enable selective
  reprocessing of resources whose content has not changed but whose block representation may improve
  (without a full policy-version reindex). **Not implemented.** The field is hardcoded to `"1"` in
  both ingestors and in `store-libsql`'s resource upsert, and the skip-check never reads it — see
  [docs/architecture.md](../docs/architecture.md#known-gaps) gap #8, which owns this gap's status.

  Conditional GET makes that gap cost more than gap #8's framing suggests, and the suppression rule
  above is why. Gap #8 describes a missed re-extraction: a parser change that leaves the extracted
  bytes identical does not re-index. With conditional headers in play the failure starts one step
  earlier and is not limited to identical output — an unchanged origin answers 304 before any parser
  runs at all, so a parser improvement cannot reach an already-indexed URL resource regardless of
  what it would have produced. The suppression rule names `extractor_version` as a designated join
  point precisely for this; until the field carries a real value there is nothing to join, and only
  a `policy_version` bump reaches those resources.

### Metadata-only update

The skip-check compares three values, not one: `content_hash`, `policy_version`, and `metadata_hash`
— a hash (`core::ids::compute_metadata_hash`) of a resource's _persisted_ metadata state:
post-title-backfill `Metadata` (a resource's own `title` folds into
`Metadata.dublin_core_mut().title` when the extracted metadata carries none) plus `external_id`,
`external_etag`, and `modified_at`. All three writers of this hash — indexing, a metadata-only
update, and rehydrating `DocumentIndex` from `RetrievalStore::list_indexed_documents` after a
process restart — derive it from that same already-persisted state, never from a resource's raw,
pre-backfill fields, so the hash means the same thing regardless of which of the three computed it.
`modified_at` is `Option<String>` ([02-domain-model.md](02-domain-model.md)'s `modified_at` row):
`None` when the source makes no change claim of its own, never our clock. A no-claim source hashes a
stable `None` on every run, so it never trips this skip-check on its own — only a genuine change to
the source's claim does, which correctly routes through the metadata-only-update outcome below, same
as any other metadata field.

Three outcomes follow from comparing incoming vs. stored state (issue #176):

- **`content_hash`/`policy_version` differ** → full reindex, as above: chunks, blocks, and
  embeddings are replaced.
- **All three match** → skip. No writes at all.
- **`content_hash`/`policy_version` match but `metadata_hash` differs** → metadata-only update:
  `RetrievalStore::update_resource_metadata` rewrites the resource row's metadata columns
  (`metadata_json`, `title`, `external_id`, `external_etag`, `modified_at`, `date_original`,
  `date_parsed`) in place. No chunk, block, or embedding write. `resources.index_updated_at` bumps
  (the store's own write-time clock, same as a full write); `resources.added_at` is untouched — the
  document wasn't re-acquired, just re-described. Counted separately from both `docs_indexed` and
  `docs_skipped`, in `IngestionResult.docs_metadata_updated` / `IndexJobStats.docs_metadata_updated`
  / the CLI summary's `docs_metadata_updated`, and reported per-document as
  `DocOutcome::MetadataUpdated`. The URI is marked `seen` exactly like an ordinary skip or a full
  reindex, so it is not eligible for the delete-sweep in the same run.

### Deletes

Deletes are data-modifying: ≥ 90% coverage gate ([01-architecture.md](01-architecture.md) §7).

**Deletion is opt-in.** `localdb index` removes nothing; `localdb index --delete` does, following
`rsync --delete`. A retaining run reports what pruning would have removed
(`IngestionResult.docs_prunable`) so nothing goes silently stale. Two reasons for the default:
removal is asymmetric (a wrong delete costs a full re-index, a missed one costs a stale hit —
[#156](https://github.com/dokterbob/localdb/issues/156) cost ~4.4M chunks), and retention is often
what's actually wanted — a local copy of a newspaper article that has since 404'd is _more_ valuable
for having outlived its origin.

Under `--delete`, two paths remove documents, and the distinction between them governs everything
below: **knowing a resource is gone is not the same as failing to find it.**

**Confirmed gone → deleted unconditionally.** An ingestor that positively establishes a locator no
longer exists at the origin (HTTP 404/410 after retry) reports it via `IngestCallback::on_gone`. The
origin was reached and answered; nothing is inferred, so no guard applies and the feed exemption
below does not either. An ingestor that merely _fails to observe_ a locator must not use this hook.

**Presumed gone → the guarded delete-sweep.** A resource stays alive across a run if it is observed
via **either** `on_resource` or `on_skipped`; the sweep removes a URI that neither hook reported.
`on_skipped` takes an already-canonical `Uri`, the same representation `on_resource`'s
`Resource.uri` carries, so both hooks populate the "seen" set in one consistent key space. Because
this infers deletion from absence, it runs only where the absence is informative:

- **Feed sources are exempt entirely.** A feed exposes a bounded window of recent entries, so an
  entry's absence means "it scrolled off," not "it was deleted."
- **Guard 1 — incomplete enumeration.** An ingestor that could not observe its source reports
  `IngestResult.enumeration = Enumeration::Incomplete { reason }`; `enumerate_path_source` returns
  `PathEnumeration::RootUnavailable` for a root that does not exist (an unmounted volume), distinct
  from `Complete(vec![])` for a root that exists and is empty. Incomplete ⇒ no sweep.
- **Guard 2 — zero-seen backstop.** A source that owns previously indexed URIs and observed none of
  them this run does not sweep, whatever the ingestor claims. Source-shape-agnostic, so it also
  covers connectors that cannot detect their own incompleteness. It does not subsume guard 1: a
  connector that enumerates 3 of 500 items before failing has a non-empty seen set, so only guard 1
  protects the other 497.

Both suppressions log a warning naming the source, the reason, and the number of documents
preserved. **Retention trade-off:** a source whose contents really were all removed at once keeps
its documents until the source is removed and re-added — accepted deliberately, and stated in the
warning.

At the resource level the same rule is a **sink invariant**: `index_resource` never deletes on an
empty replacement. A resource that chunks to nothing returns `IndexOutcome::Empty` — nothing
written, nothing deleted — and the pipeline records it as a skip without touching `DocumentIndex`.
Ingestors should still classify "extracted to nothing" as unusable and report it via `on_skipped`
(`FileIngestor` and `UrlIngestor` both do), but that is defense in depth; the guarantee lives at the
sink, where no ingestor can bypass it. **Retention trade-off:** a file legitimately emptied keeps
its previous content indexed. The escape hatch already exists — delete the file, and the sweep
removes it normally under `--delete`.

[#156](https://github.com/dokterbob/localdb/issues/156) (source level, zero URIs enumerated) and
[#185](https://github.com/dokterbob/localdb/issues/185) (resource level, zero blocks extracted) are
one conflation — "unavailable" mistaken for "legitimately empty" — one layer apart.

### Aged-out feed entries: the liveness sweep

The liveness sweep sits in the **confirmed-gone** bucket above, not the presumed-gone one — it never
deletes on absence, only on a positively confirmed 404/410. The feed connector's exemption from the
presumed-gone sweep is unchanged by this and remains correct: an entry scrolling off the feed window
is still never, on its own, treated as a deletion signal.

It runs **only** for `SourceSpec::Feed` sources, and **only** under `DeletionPolicy::Prune`
(`--delete`). Unlike the ordinary delete-sweep, it can only learn anything by making a network
request against each candidate's stored link, so — stated explicitly — there is no free
`docs_prunable` preview signal for it on a retaining run: a retaining run performs zero liveness
fetches, and reports nothing pruned or prunable for this mechanism.

**Candidates:** resources with `ingestor_kind = 'feed'` and a non-`NULL` `external_id` that this run
did not observe, ordered oldest `last_checked_at` first, with never-checked resources leading. It is
bounded twice: a batch cap of **25 candidates per run per source**, and a recheck floor of
`max(refresh_interval, 24h)` below which a resource is not re-probed at all, regardless of how long
it has gone unobserved. A feed source with no `refresh_interval_secs` configured — the common case —
therefore uses the bare 24h floor.

**"Did not observe" is not "aged out of the window" (normative).** This section is named for the
case it exists to serve, not for a precondition it can enforce. A run that read the feed's window
and did not see an entry did watch that entry age out; a run whose feed document answered 304 fires
zero entry callbacks and observed _nothing_, so every one of that source's entries is a candidate,
including entries the window still lists. Nothing persists window membership, so the two cases are
indistinguishable to the candidate query, and the sweep does not pretend otherwise. What makes that
safe is that absence only selects who gets _probed_: the delete needs a 404/410 confirmed by the
entry's own origin, and the mechanism as a whole needs `--delete`. The cost it accepts — an entry
pruned this way is recreated from the feed's embedded content on the next run where the feed
document changes, then pruned again — is stated with its ticket in
[02-domain-model.md](02-domain-model.md) §2, "Conditional GET and pruning".

**The feed's own document is never a candidate.** In single-document mode
(`fetch_full_content: false`) the feed document itself is stored as a resource, under the feed URL,
with `ingestor_kind = 'feed'` — and it is the one such resource carrying no `external_id`, since
every discovered entry is stamped with the entry's own id. Were it a candidate, a 404/410 on the
feed URL would delete the source's entire index through a mechanism meant to prune a single entry,
so the candidate query requires `external_id IS NOT NULL`. A legacy row whose `external_id` was
never captured is excluded by that same predicate and can therefore never be pruned by this
mechanism: retention bias is the safe failure direction here, as it is throughout "Deletes" above.

The batch cap counts candidates **actually probed**, not rows the store returned. The oldest-first
query cannot see the run's in-memory seen-set, so applying the cap in SQL alone would let a run
whose freshly-observed entries happen to sort oldest fill the whole batch with entries it had just
seen and probe nothing at all. The query therefore over-fetches by the size of the seen-set, bounded
by a fixed ceiling well above any realistic feed window, and the caller subtracts the seen-set
before taking its 25. The ceiling is what keeps the query bounded: `max_entries` is optional and
defaults to unbounded, so the seen-set has no principled size of its own.

**Fragment URIs are never candidates.** A link-less entry is stored under a synthetic
`{feed_url}#entry:{id}` URI ([02-domain-model.md](02-domain-model.md), "General connector pattern").
HTTP never sends a fragment on the wire, so probing that URI verbatim would actually request the
feed root, not the entry — a 404/410 there would delete the entry's resource on a signal that has
nothing to do with it. The candidate query excludes every URI carrying a `#`, which also excludes a
_real_ entry link that legitimately carries a fragment (`https://example.com/post#section`): that
entry can never be pruned by this mechanism. Both exclusions are deliberate and correct in the same
direction as the "Deletes" trade-off above — retention bias is the safe failure.

**Only `http`/`https` URIs are candidates.** A feed entry's `<link>` need not be an HTTP URL —
`mailto:` and `ftp:` parse fine, and such an entry is indexed from its embedded content under that
very URI. Handing one to the HTTP fetcher can only fail, so it is never a wrong delete; the cost is
that it burns one of the run's 25 probe slots, on every run for as long as the entry stays aged out,
on a request that could not have resolved anything. The candidate query filters by scheme for the
same reason it filters fragments: an unprobeable URI should never become a candidate in the first
place.

**Guards (normative).** The sweep inherits **one** of the presumed-gone sweep's two guards, not
both, because the two sweeps infer different things from the same seen-set. The presumed-gone sweep
deletes on absence, so an untrustworthy seen-set is an untrustworthy delete signal. Here absence
only selects who gets _probed_; the delete needs a confirmed 404/410 from the origin. Probing an
entry that is in fact still in the window costs one conditional request and deletes it only if that
entry's own origin says it is gone ("Did not observe" above).

- **Guard 1 — incomplete enumeration — still suppresses the sweep entirely.** A run that could not
  read the source's window knows nothing about which entries it holds, so every previously indexed
  URI would become a candidate and the source's whole document set would queue for probing, 25 per
  run, off a signal already known to be broken.
- **Guard 2 — the zero-seen backstop — does not apply.** A run whose feed document answered 304
  fires zero entry callbacks, so its seen-set is empty ([02-domain-model.md](02-domain-model.md),
  "Conditional GET and pruning"); the sweep runs anyway. Suppressing it there starves the mechanism
  in precisely the case it exists for: a feed goes quiet, its document stops changing, every
  subsequent run 304s, and the aged-out backlog is never probed again — the sweep being the only
  thing that could ever shrink it. Running is safe because both bounds that make the sweep safe at
  all are independent of the seen-set: it deletes only on a confirmed 404/410, and it probes at most
  25 candidates per run per source, none more often than the recheck floor allows. An empty seen-set
  subtracts nothing from the candidate list, so a 304'd run offers every one of the source's entries
  as a candidate, window members included — the direct consequence of having no membership record to
  consult, bounded by the delete rule rather than by the candidate rule ("Candidates" above).

**Log level.** Both guards suppress the presumed-gone sweep at `warn` for path/url sources — see
"Deletes" above. For the feed liveness sweep specifically, only the incomplete-enumeration guard
stays at `warn`; the zero-seen backstop logs at `debug`, since for a feed under `--delete` its
overwhelmingly common cause is the routine feed-document 304 described above, and warning on every
steady-state run trains operators to ignore the level.

**Ordering:** the sweep runs **after** the source's ordinary ingestion pass completes, so the
seen-set it partitions against is final and no entry a run observed is ever probed on the run that
observed it.

**Per candidate**, the resource's stored validators are replayed against its link:

- `Gone` (404/410) deletes the resource.
- `NotModified` refreshes the stored validators and `last_checked_at`, but does **not** delete — the
  entry is still there.
- A `200` advances `last_checked_at` **only**, rewriting the candidate's already-stored validators
  unchanged. The fresh validators the response carried are deliberately discarded, because the body
  they describe is discarded too (see below). Storing them would point the resource's validators at
  a representation this store never indexed: the next probe would answer 304, and if the entry ever
  re-entered the feed window that 304 would suppress the reindex of the changed content
  indefinitely. The price is that a genuinely-changed aged-out entry keeps paying a full `200` at
  every recheck instead of converging on a cheap 304 — pure overhead on an entry the sweep does not
  re-index anyway, and the correct side to err on.
- `Blocked` or a transport error is not evidence about the entry, so nothing about the resource's
  content, metadata, or validators moves.
- A store write failure for one candidate (e.g. a concurrent delete racing the probe) is logged and
  the sweep moves on to the next candidate — it does not abort the batch and discard the stats
  already computed for the candidates processed alongside it.

**What a probe writes (normative).** Every outcome above except the delete converges on a single
`RetrievalStore::touch_resource_liveness` call, which writes exactly three columns —
`external_etag`, `external_last_modified`, `last_checked_at` — and nothing else. It is deliberately
**not** a metadata-only update ("Metadata-only update" above) and must not be routed through that
contract: `resources.index_updated_at` does not bump, no `docs_metadata_updated` is counted, and no
`DocOutcome` is emitted. A probe writes no content and no metadata, and `index_updated_at` means "we
last wrote this resource's stored state" — public as `DocumentInfo.index_updated_at`
([02-domain-model.md](02-domain-model.md) §2) — so bumping it would report a document as re-written
when it was only pinged. Nothing desyncs by leaving it alone, because `metadata_hash` is never
persisted (§2's `content_hash` row: it is compared against "a third, unstored value"). It is
re-derived from `metadata_json` + `external_id` + `external_etag` + `modified_at` on every
rehydration, so a rotated `external_etag` simply changes what the _next_ rehydration computes. The
run's own in-memory copy is not read again either: the sweep runs last in its source's pass
("Ordering" above) and touches that index only to drop a candidate it deleted.

**`last_checked_at` means "when we last attempted a probe" (normative)** — not "when we last
successfully reached the origin". It advances on **every** outcome above except the delete, blocked
and transport-error outcomes included, and it is the only thing those two outcomes move. That is
what makes the oldest-first ordering a fair rotation: were an unreachable candidate left holding its
old timestamp, a source with 25 or more permanently-blocked entries would lead the query forever and
no other candidate would ever reach a batch.

A `200` here is **not** re-indexed — deliberately out of scope. An aged-out entry's feed-sourced
metadata (title, author, per-entry publication date) is long gone once the entry has fallen out of
the feed window, and re-indexing from the bare page alone would silently degrade the stored resource
rather than improve it.

Entry links are fetched through the **public-destination-only** fetcher — the same SSRF trust
boundary the feed ingestor already applies to entry links ([02-domain-model.md](02-domain-model.md),
"Destination policy (entry links)") — never the unrestricted fetcher used for the
operator-configured feed URL itself.

Prunes fold into the existing `docs_deleted`, which stays a single total: a run does **not** report
how many of its deletions the liveness sweep caused. The sweep's own counter,
`feed_entries_liveness_checked`, counts probe _attempts_ — carried per source in
`IngestionResult.feed_entries_liveness_checked`, summed across a run's sources into
`IndexJobStats.feed_entries_liveness_checked`, and reported as the CLI summary's
`feed_entries_liveness_checked`, the same three-step channel `docs_metadata_updated` travels
("Metadata-only update" above). It shows the work done even on a run that deleted nothing, but it
cannot be subtracted from `docs_deleted` to attribute a cause, since most attempts end in a 304 or a
200 rather than a delete. Deletion-cause attribution needs a counter of its own and is deliberately
left out.

## 2. Extraction (v1 matrix)

The parser chain is an implementation detail of the `file` and `url` ingestors. Parsers still return
a `ParsedDocument` (Markdown string + title + `DocumentMetadata`), which the ingestor then converts
to a `Resource` with typed blocks via `markdown_to_blocks()`. Future ingestors that natively produce
structured content may emit blocks directly without going through a Markdown intermediate.

| Format                 | Approach                                                                             | Notes                                                                                                                                    |
| ---------------------- | ------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Markdown               | pulldown-cmark parser (passthrough)                                                  | Headings → heading blocks; code fences → code blocks.                                                                                    |
| Plain text             | direct passthrough                                                                   | Treated as Markdown verbatim.                                                                                                            |
| HTML                   | readability-style main-content selection → Markdown                                  | Used for both `url` fetches and `.html` files.                                                                                           |
| PDF (text layer)       | `pdf_oxide` per-page → Markdown, plus retrieval-oriented post-processing (see below) | Scanned/text-less PDFs are rejected (no OCR); Info dict + XMP → `DocumentMetadata`.                                                      |
| Office (DOCX/PPTX/CSV) | `anytomd` (v1.3.0) → Markdown                                                        | Production-ready. XLSX/XLS disabled (see below).                                                                                         |
| EPUB                   | `rbook` spine walk → per-chapter XHTML → Markdown via the internal HTML converter    | Reading order preserved; OPF Dublin Core → `DocumentMetadata`. Extension-gated (`.epub`). DRM'd / image-only books → `ExtractionFailed`. |

#### PDF extraction is tuned for retrieval, not for visual fidelity

A PDF is converted one page at a time and the pages are concatenated, recording the byte offset
where each page begins so blocks can carry a `page` number (§2 of
[02-domain-model.md](02-domain-model.md)). Four deliberate deviations from the extractor's defaults,
plus three local repairs, exist because the goal is the words an author wrote — not a faithful
reproduction of the printed page:

- **Artifacts are dropped** (`/Artifact`-tagged running headers, footers, page-number folios and
  watermarks, per ISO 32000-1 §14.8.2.2.1). Without this, a folio becomes its own chunk.
- **Ligatures are expanded** (U+FB00–U+FB06 → `fi`/`fl`/`ff`/…), so BM25 tokenization and the
  embedder see real words.
- **Soft hyphens (U+00AD) are deleted.** They are discretionary line-break hints with no textual
  meaning (§14.8.2.2.3). Deletion alone reconstructs the word; line-break hyphens are deliberately
  _not_ rejoined, as that is where `well-being` → `wellbeing` corruption comes from.
- **Pages with no text layer are dropped, not annotated.** A page that yields nothing contributes
  nothing; the whole set is reported once via a `WARN` naming the count and page ranges. A _mixed_
  document — real text on some pages, bare scans on others — still indexes its text. This is what
  keeps the "scanned PDFs are rejected, OCR is out of scope" statement below honest: previously a
  placeholder marker was emitted per skipped page and indexed as though it were content, which also
  let mixed documents evade scanned-PDF detection entirely.
- **Heading and code-block inference is guarded.** The extractor infers headings from font
  clustering and code blocks from monospace detection; both over-fire on real books. A heading that
  fails a sanity check is demoted to a paragraph (it stays indexed — it just stops becoming a
  `heading_path` breadcrumb for every following chunk), and a bare fence whose content reads as
  prose is un-fenced. Both guards are biased hard against false positives.

Unmappable glyphs never reach the index as mojibake. A Type0/Identity-H font carrying no
`/ToUnicode` CMap and no embedded font program gives the extractor no way to recover characters, so
extraction either fails outright or returns text containing no U+FFFD replacement characters — it
never emits the replacement-character soup that leaves a document looking indexed while being
unsearchable. `extract/tests/fixtures/malformed/cid_no_tounicode.pdf` pins the single-document case,
and the corpus test forbids U+FFFD across every fixture.

Geometric stripping of running headers in _untagged_ PDFs is **not** enabled: the upstream
implementation matches glyph-run spans rather than lines and deletes body text from multi-column
documents. Reported upstream as
[pdf_oxide#1022](https://github.com/yfedoseev/pdf_oxide/issues/1022).

**Out of scope (explicit):** OCR / scanned PDFs and images. EPUB is the only ebook format supported;
**MOBI/AZW/AZW3** (PalmDOC/KF8 compression, frequent DRM — realistically need a Calibre shell-out)
and **FB2/CBZ** (on `rbook`'s roadmap, not yet implemented) are deferred. Rationale and the full
deferred list: [06-roadmap.md](06-roadmap.md) §5. Unsupported files are skipped and counted in
IndexJob stats, not errors.

**XLSX/XLS explicitly disabled:** Despite anytomd supporting XLSX/XLS in principle, extraction for
these formats is disabled in `OfficeParser` pending an upstream performance fix.
`anytomd::convert_bytes` on an 87K-row XLSX (6.9 MB) took >16 minutes in production (vs. <1 s for
the equivalent CSV). The file is counted as `unsupported_format`, not an error. Use CSV export as a
workaround. Tracking: <https://github.com/developer0hye/anytomd-rs/issues/94>

**Extension-gated acceptance:** The `PlaintextParser` (and by extension the full parser chain) only
accepts files whose extension or basename matches the list published by
`extract::supported_extensions()` (text and code/data extensions, plus known lockfile basenames such
as `Cargo.lock`). Files with unknown or binary extensions — `.exe`, `.png`, `.bin`, etc. — are
declined at the parser level and counted as `unsupported_format` in `IndexJobStats` without ever
entering the chunker or embedder. This prevents indexing hangs caused by chunkers receiving
arbitrarily large binary blobs.

**Default include allowlist for directory sources:** When a `path` source points to a directory and
no explicit `include` globs have been set, `cli` automatically applies `DEFAULT_PATH_INCLUDES` — a
glob list derived from `extract::supported_extensions()` — so that file-system enumeration skips
unsupported files before they ever reach the extraction layer. Single-file sources are not affected
(they carry an exact filename glob). Sources added via explicit `include` override this default
entirely.

**Three-way per-document classification:**

| Outcome                                               | Error variant                   | Counter                    | Behavior                                  |
| ----------------------------------------------------- | ------------------------------- | -------------------------- | ----------------------------------------- |
| Format not handled (e.g. scanned PDF, binary `.html`) | `UnsupportedFormat`             | `unsupported_format_count` | Silent; no WARN.                          |
| Supported format, broken instance (e.g. corrupt DOCX) | `ExtractionFailed`              | `error_count`              | WARN logged per file; counted as failure. |
| Unexpected panic in parser/chunker                    | `Internal` (via `catch_unwind`) | `error_count`              | WARN logged per file; counted as failure. |

In all three cases the ingestion loop continues with the next file; the process does **not** abort.

Exactly one WARN is emitted per failing resource. `core::ingestion` owns that line — it is the layer
that accounts for ingestion outcomes — so ingestors log their extra framing at `debug!` and fold
anything the operator needs at default level (e.g. that the failure was a _panic_ rather than a
returned error) into the `SkipReason::Error` payload itself.

**Partial success is a fourth, orthogonal case.** A document can extract successfully and still lose
content: a PDF whose text layer covers only some pages contributes nothing for the rest. That is
`Ok` — the document indexes, and counts as a success — but `extract` emits one WARN naming how many
pages were dropped and which. It is deliberately not an error variant: a mixed scanned/text book
must still be indexed for the text it does have.

**`--strict` opt-in:** by default `index` is best-effort (exits `0` regardless of per-file
failures). Pass `--strict` to exit `2` after the run completes when `error_count > 0`. Unsupported
files do not trigger `--strict`; only `ExtractionFailed` / `Internal` errors do.

**Binary / non-UTF-8 input:** All parser implementations (`MarkdownParser`, `HtmlParser`,
`PlaintextParser`) decline non-UTF-8 bytes by returning `Ok(None)` rather than
`Err(InvalidRequest)`. A file with a recognized extension that contains binary or mis-encoded bytes
therefore falls through the entire parser chain and is counted as `unsupported_format` in
`IndexJobStats`, not as an error.

**Per-document panic isolation:** `index_document` wraps the synchronous extraction and chunking
calls in `std::panic::catch_unwind`. Any unexpected panic in a parser or chunker is caught,
converted to `Err(Error::Internal)`, logged as a per-file WARN, and counted in `error_count`. The
ingestion loop continues with the next file; the process does not abort.

## 3. Chunking

Chunking operates on **blocks**, not on a raw Markdown string. Each resource carries an ordered
sequence of typed blocks; the chunker dispatches on `BlockKind` to produce block-appropriate chunks.

### Block-dispatch rules

| Block kinds                                       | Chunker             | Behavior                                                                                                                                                                        |
| ------------------------------------------------- | ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Heading`, `Text`                                 | prose chunker       | `MarkdownSplitter` subdivides within the block on semantic boundaries (sentences, words); target ≈ 256 tokens, overlap ≈ 0 tokens.                                              |
| `Code`                                            | code chunker        | Line-based subdivision within the block; target ≈ 60 lines.                                                                                                                     |
| `Message`, `Segment`                              | messages chunker    | Multi-block windowed: sliding window over consecutive Message/Segment blocks (see below).                                                                                       |
| `Table`                                           | table chunker       | Row-based packing: rows are packed into chunks under the token target; every chunk re-emits the header row and separator so each chunk is a standalone valid table (see below). |
| `Reference`, `Attachment`, `Frontmatter`, `Image` | one chunk per block | These block types are typically small; no further subdivision.                                                                                                                  |

The source-level preset (from config or the `--preset` CLI flag) acts as a hint for ambiguous cases;
block-kind dispatch takes precedence for unambiguous kinds.

**Invariant:** chunks never cross block boundaries, EXCEPT message-window chunks, which explicitly
span multiple consecutive `Message`/`Segment` blocks. This invariant makes `heading_path`
attribution deterministic (see below) and ensures context-expansion queries are well-defined.

**Coarse `Text` blocks:** `markdown_to_blocks` emits **one** `Text` block per run of consecutive
running-text content — paragraphs, lists, blockquotes, HTML blocks — between structural boundaries
(a `Heading`, a `Table`/`Code`/`Image` block, or the start/end of the document). Block sizing is
therefore purely structural, driven by element boundaries, not by a token target; the prose chunker
restores token-sizing by splitting a `Text` block into one or more prose chunks via
`MarkdownSplitter`. This is what lets prose chunks approach the ~256-token target instead of
producing one tiny chunk per paragraph (the #158 fix) — a multi-paragraph run of running text is
chunked together, not paragraph-by-paragraph. **Headings remain discrete blocks and are still
chunked** — a `Heading` block is never folded into an adjacent `Text` run, because it feeds the
contextual embedder's document context and `heading_path` attribution (see below). Filtering
headings out of _search results_ is a separate, future search-side concern, not a chunking-time
decision.

### heading_path attribution

`heading_path` is derived from the block tree: heading blocks that precede a content block in the
resource's ordered block sequence are collected into the path for all chunks produced from that
content block. There is no re-parsing of Markdown — the heading structure comes directly from the
block representation.

### Source preset override

`IngestionConfig` carries a `source_preset` field (`prose` (default) | `code` | `messages`),
resolved from the source's config (the source-level `preset` key, [03-config.md](03-config.md) §2)
or the CLI's `--preset` flag. Per-file automatic preset routing — `preset_for`, which inspects a
resource's `uri` and `mime` hint to route e.g. JSON/YAML/lockfiles to the `code` preset — applies
**only** when the source preset is the default `prose`. An explicit `code` or `messages` source
preset is authoritative and wins over per-file detection: a source configured as `code` chunks every
file in it with the code chunker regardless of what `preset_for` would otherwise guess, and likewise
a `messages` source always uses the messages chunker. This preserves useful per-file auto-detection
for the common (prose) case while giving explicit non-default presets full, unambiguous control —
needed for message/transcript sources where `preset_for`'s filename/mime heuristics do not apply.

### Messages chunker

The `messages` preset is implemented as a sliding window over `Message`/`Segment` blocks:

- **`window_turns`** (default 6): number of consecutive turns per chunk.
- **`stride_turns`** (default 3): step between windows.
- Windows are additionally bounded by token count so that no single chunk exceeds the embedding
  model's context limit.
- Each window chunk carries the `heading_path` of the containing thread/resource.
- **`window_block_seqs`:** each window chunk records the `block_seq` of every member block it spans
  in its `location` (`location_json = {start, end, window_block_seqs?}`), not just the denormalized
  `block_seq` of its first member — so context-expansion and `get_chunks` consumers can resolve
  every block a multi-block chunk touches.
- **Chunk ids after fix-up:** chunk ids are computed **after** the window fix-up pass (the pass that
  shrinks a window from the end to fit the token budget, or to keep the last window from running
  past the end of the turn sequence — see `chunk_messages`) — so the content-addressed id (§4 below;
  [02-domain-model.md](02-domain-model.md) §3) reflects the window's final membership, not a
  pre-fix-up candidate window.

### Table chunker

`Table` blocks are chunked by a dedicated row-based packer, not routed through the code chunker:

- Rows are packed greedily into successive chunks, filling each chunk up to the (prose) token
  target.
- Every chunk re-emits the table's header row and the `|---|` separator row, so each chunk is a
  valid, independently renderable Markdown table — a chunk is never dependent on a sibling chunk to
  parse correctly.
- **Oversized single row:** a single data row that alone exceeds the token target cannot be packed
  into a standalone valid chunk at the target size; it falls back to `chunk_code`'s long-line split
  (see below), preserving the invariant that no single chunk grows unbounded.

### Prose chunker details

Chunk sizing for `prose` is **token-accurate**, measured using the embedding model's own tokenizer
(the default model `pplx-embed-context-v1-0.6b` supports up to 32K tokens; localdb caps its
late-chunking window at 4096 tokens = 16 × 256-token chunks). When no local tokenizer is available
(e.g. hosted/API embedders), it falls back to a character approximation (~4 chars/token). The
256-token / 0-overlap defaults mirror the contextual late-chunking model's training regime:
Perplexity's contextualized-embeddings model is trained on documents partitioned into 256-token
chunks (16 chunks per 4096-token document) with **no** intra-document overlap, because late chunking
shares context across chunks from the same document (chunks must be sent in source-document order)
and so supplies cross-chunk context itself. Aligning the chunker to that regime gives smaller,
precise chunks — better citation granularity — while the model handles cross-chunk context, with no
overlap needed. These are defaults to beat with evaluation, not dogma.

**`chunk_prose` structureless fallback:** Before invoking `MarkdownSplitter`, `chunk_prose` runs two
O(n) probes over the block; tripping either delegates the whole block to the `code` chunker so the
content is still indexed in bounded chunks:

- **Quality probe:** longest whitespace-free run > `STRUCTURELESS_RUN_MULTIPLIER` (8) × the target.
  An ordinary paragraph or long-lined prose block in a whitespace-delimited script has plenty of
  internal whitespace and stays on the prose path; genuinely structureless content (minified JSON, a
  lockfile) does not (#191, #192).
- **Performance guard:** longest _line_ > `OVERLONG_LINE_MULTIPLIER` (64) × the target, whitespace
  or not. `MarkdownSplitter`'s split-point search is super-linear on a single flat line, so a
  pathologically long line (hundreds of KB with no newlines) must not reach it — the
  multi-minute-hang class the fallback was originally introduced for (#61). Real prose paragraphs,
  even the single-line ones EPUB/HTML extraction emits, stay far below this cap.

Both probes compare a _char_ count against the target, which in production is a _token_ count from
the embedder's tokenizer — a deliberate heuristic, not an exact unit match. The chars-per-token
ratio is tokenizer-dependent: for whitespace-delimited text it is ≥ 1, so the probes trip no later
than a true token measure would; BPE tokenizers can emit multiple tokens per char on CJK and rare
scripts, inverting the direction — an accepted imprecision for content already covered by the CJK
limitation above. Known limitation: scripts without inter-word whitespace (CJK, Thai, …) make a
whole paragraph one "run", so long CJK prose trips the quality probe and gets char-aligned cuts in
the `code` chunker; proper word segmentation is out of scope.

**`chunk_code` long-line split:** `chunk_code` enforces a per-line char limit. Lines exceeding the
limit are hard-split into target-sized pieces, preventing single-line binary or minified content
from producing unbounded chunk sizes. Each cut prefers the last whitespace inside the window over
the raw char boundary — as long as backing off to it still leaves a piece more than half the window
— so ordinary long-lined prose splits between words instead of mid-word; content with no whitespace
in the window (base64, URLs) still gets the hard char cut (#191).

### Spreadsheet routing

Spreadsheet formats (`.xlsx`, `.xls`) route to the code chunker. These files produce extracted text
that is dense tabular content (similar to CSV), so the fast line-based chunker is used instead of
the prose splitter to avoid hangs on large tables. Note: XLSX/XLS extraction is currently disabled
(see §2), so this routing is moot until the upstream fix lands.

## 4. Embedding

### Document-aware interface

**Decision:** the `Embedder` trait in `core` receives **chunks grouped by resource, with resource
and block context** — not a flat list of strings:

```
embed_documents(docs: [{document_context, chunks: [chunk_text, ...]}, ...])
    -> [[vector, ...], ...]
```

"Document context" for embedding is constructed from the resource's block sequence. Concretely, an
embedding renderer may serialize nearby blocks into a Markdown-like context string as an
implementation detail — the trait shape is stable regardless of how context is assembled. Classic
per-chunk embedding is the degenerate case (context ignored, one chunk per call batch).
**Rationale:** contextualized/late-chunking models need the surrounding document to embed each
chunk; retrofitting a flat trait later would touch every call site. The message-store case (thread
as context for each turn window) is the same shape ([02-domain-model.md](02-domain-model.md) §5).
**Rejected:** flat `embed(texts) -> vectors` trait — locks the architecture to context-free
embedding.

### Models and providers

| Role                          | Choice                                                               | Notes                                                                                                                                    |
| ----------------------------- | -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| **Default (headline)**        | `pplx-embed-context-v1-0.6b`, local via ONNX                         | Open-weight, MIT, explicit late-chunking support (verified mid-2026). Confirmed as default; see benchmark section for performance gates. |
| Lightweight preset / fallback | bge-small-class dense model                                          | For weak hardware; classic per-chunk path.                                                                                               |
| Hosted contextualized         | Perplexity `/v1/contextualizedembeddings`; Voyage `voyage-context-3` | Same nested API shape as the trait — direct mapping.                                                                                     |
| Generic hosted                | Any OpenAI-compatible `/v1/embeddings` endpoint                      | Degenerate (flat) path; one provider abstraction for embeddings, LLMs stay out of the core process entirely.                             |

Models are **downloaded on first run** (with progress UI, checksum verification, resumable) into the
model cache ([03-config.md](03-config.md) §4) — never bundled into the binary.

### Local backends: ONNX (CPU) and CoreML (ANE/GPU)

The default `pplx-embed-context-v1-0.6b` runs on two interchangeable local backends, selected by the
`local` / `local-coreml` / `local-onnx` provider values ([03-config.md](03-config.md) §7).

- **ONNX (CPU):** the reference path. Late-chunking is run in `embed`: the model emits token
  embeddings, then Rust does mean-pooling over each chunk's token span and `tanh` int8 quantization
  before binarization.
- **CoreML (ANE/GPU):** macOS-only, behind the opt-in `local-coreml` cargo feature (requires Rust ≥
  1.85, subsumed by the workspace's 1.88 floor). Executes on Apple Silicon's ANE/GPU via
  `objc2-core-ml`. Pooling and `tanh` int8 quantization happen **inside the model** — it consumes a
  `pool_matrix` input and outputs int8 `(32, 1024)` directly, so the in-Rust mean-pool + quant of
  the ONNX path is not needed. The CoreML bundle is the context (late-chunking) variant, downloaded
  from HF repo `dokterbob/pplx-embed-coreml` (pinned revision) via `hf-hub` 1.0, whose built-in XET
  transfers deduplicate the shared ~1.15 GB encoder weights across sequence-length buckets. Buckets
  are fixed ANE sequence lengths `L ∈ {512, 1024, 2048, 4096}` (whichever are published — currently
  only `context/L512-int8`) plus an optional dynamic GPU catch-all.

Both backends are **index-interchangeable**: same `model_id`, 1024-dim, `Binary` encoding,
sign-compatible vectors. Measured on Apple Silicon (CoreML fp16/ANE vs ONNX fp32/CPU on identical
chunks): **cosine parity ~0.995–0.9995** (the full-precision direction is essentially identical),
and **per-dimension sign/Hamming agreement ~98–99%** (0.982–0.994 observed). The few flips (~5–11 of
1024 dims) are dimensions whose pre-tanh value sits within fp16-rounding distance of zero and so
round to a different int8 sign at the tie point — they carry negligible magnitude. An index built by
one backend is queryable by the other with no reindex (the choice of backend does not affect
`policy_version`); cross-backend Hamming distances carry ~1–2% backend-induced bit noise on
near-zero dimensions, which is small relative to inter-document distances.

### Gating benchmark for the default model

Before `pplx-embed-context-v1-0.6b` is confirmed as default, measure on a mid-range laptop (Apple
Silicon, 16 GB): index a ~2 000-file / ~100 MB mixed corpus. **Gate:** sustained ≥ 15 chunks/s
end-to-end and first-index ≤ 30 min; if missed, the bge-small-class preset becomes the default and
the 0.6b model the opt-in quality preset. Either outcome is config, not architecture.

### Real-corpus reference point

The gate above is a synthetic target. For a sense of what a real store looks like, an actual
PDF-heavy mixed corpus of **1 063 documents indexes to roughly 642 000 chunks** — on the order of
600 chunks per document. That ratio, not the document count, is what sets the cost of a full
reindex: anything that changes `policy_version` re-embeds every one of those chunks, so on a store
this size a policy or chunking-algorithm bump is a substantial one-time operation to be scheduled
rather than triggered casually.

### Policy versioning

`policy_version = hash(canonical serialization of the store's effective {chunking, embedding, parsers})`.
Stored on every chunk. On store open / config change, if the effective policy hash differs from the
indexed one, the store is marked stale and a reindex job is created (daemon: automatic; embedded: on
next `localdb index`, with a warning from `status`). Chunker, embedder, and parser list change
**together** — there is no partial invalidation ([03-config.md](03-config.md) §2). The `parsers`
list is hashed **order-sensitively** (unlike `chunking`/`embedding`, which use order-independent key
serialization), so reordering parsers alone marks the store stale and schedules a reindex.

The `chunking` sub-policy embeds a chunking algorithm identifier as part of what gets hashed;
bumping it forces a reindex even when no user-visible config field changed. Current value:
**`textsplitter-md-v6`** (bumped from `v5`). The `v6` bump covers the mid-word-split fix (#191,
#192): `chunk_prose`'s structureless backstop is now dual-condition — longest whitespace-free run >
8× target (quality) or longest line > 64× target (performance guard, replacing the old 8× line
probe) — and `chunk_code`'s overlong-line hard-split now backs off to the last whitespace in the
window (falling back to the char cut only when the window has no whitespace) — chunk boundaries
change for long single-line prose, so existing stores reindex on the next `localdb index`. The `v5`
bump covers the coarse `Text` block ontology — `markdown_to_blocks` now emits one `Text` block per
run of consecutive running-text content, so prose chunks pack toward the ~256-token target instead
of one-tiny-chunk-per- paragraph (§3 above, "Coarse `Text` blocks"; the #158 fix), silently altering
chunk boundaries. The prior `v4` bump (from `v3`) covered two changes at once: the new `Chunk.id`
formula — `blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)`
([02-domain-model.md](02-domain-model.md) §2, Chunk) — and the addition of the table chunker. Any of
these changes alone would silently alter chunk boundaries and/or ids without a policy bump,
defeating incremental re-index's staleness detection.

`content_hash` is a blake3 hash of the ordered canonical texts of all blocks in a resource (not of a
Markdown string). `extractor_version` on resources enables selective reprocessing when parser or
`markdown_to_blocks()` logic improves, without requiring a full policy-version reindex.

## 5. Retrieval

**Decision:** hybrid **BM25 + dense, fused with RRF** (k = 60), implemented **in our code** above
the `RetrievalStore` trait: query both legs (top-K each, default K = 50), fuse, then shape results.

**Rationale:** hybrid-by-default is a day-one requirement; RRF is robust, parameter-light, and
score-scale-free. Owning fusion keeps it identical across future backends. **Rejected:** score
interpolation (needs per-model calibration); backend-native fusion (backend-dependent behavior).

- **Filtering:** store filter (one, several, or all stores). Multi-store queries fan out per-store
  BM25 + dense queries, then pool each leg's results across all queried stores into one globally
  rank-ordered list _before_ a single RRF pass runs over the two pooled legs — never per-store RRF
  followed by a merge, which would let every store's local rank-0 chunk tie regardless of true
  quality (RRF scores are rank-based and scale-free). Fusion identity is the composite
  `(store_id, chunk_id)`, not `chunk_id` alone: chunk IDs are content-addressed
  ([02-domain-model.md](02-domain-model.md) §2), so the same document indexed into two stores yields
  the same chunk_id in both — a `chunk_id`-only key would silently merge two stores' distinct hits.
  Ties are broken deterministically: `fused_score` descending, then `store_id` ascending, then
  `chunk_id` ascending. Metadata filters (mime, path prefix, date-axis range) are pushed down to the
  backend where supported.

  Filter values are always passed as bound SQL parameters, never interpolated into query text; a
  literal `%` or `_` in a URI-prefix filter is treated as a SQL `LIKE` wildcard.

  A date-range filter (`MetadataFilter::DateAfter`/`DateBefore`) names one of the four `DateAxis`
  values — `added`, `updated`, `modified`, `document` (see [02-domain-model.md](02-domain-model.md)
  §2's "Date axes (normative)") — and both bounds are inclusive (`>=`/`<=`). Multiple filters, of
  any kind and in any combination, always AND together; there is no OR. A `None`/`NULL` axis value
  on a given chunk fails every bound, in both directions — a document with no claimed `modified_at`
  never matches a `modified`-axis filter regardless of the bound supplied. A `DateBefore` bound is
  widened to the latest instant its own precision allows before comparing, on **every** axis, so
  that `added_before: "2026"` includes all of 2026 rather than excluding it; the stored value is
  widened too on the `document` axis alone, the only one whose column can hold a partial-precision
  value. See [02-domain-model.md](02-domain-model.md) §2's "Date axes (normative)" for the full
  rule.

  **Surface wiring (`core::search_filters::SearchFilters`).** The CLI (`localdb search`), HTTP
  (`POST /v1/search`), and MCP (`search` tool) surfaces all build `MetadataFilter`s through one
  shared type, `SearchFilters::into_metadata_filters`, rather than each hand-rolling its own
  translation (specs/01-architecture.md §1: no domain logic in surface crates). It carries `path` (→
  `UriPrefix`), `mime` (→ `Mime`), and eight `Option<String>` date fields — `{axis}_after` /
  `{axis}_before` for each of the four `DateAxis` values — all single-valued, never a list, so a
  repeated flag is a usage error rather than silently matching nothing. `path` and `mime` are
  matched as literal strings with no date/duration parsing at all.

  Each date field accepts one of three forms, tried in this order: a full RFC 3339 datetime
  (normalized to canonical UTC), a partial date (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`, passed through
  unchanged — load-bearing for the `document`-axis widening above), or a relative duration (e.g.
  `7d`, `30m`, `2w`). A duration always resolves to `now − duration`, identically for either bound
  direction: `--modified-after 7d` means "modified within the last 7 days", and
  `--modified-before 7d` means "modified more than 7 days ago" — never `now + duration`. A value
  matching none of the three forms is `invalid_request` (exit 2 / HTTP 400 / MCP tool error), naming
  the offending field.

  **Known limitation — cross-store score comparability.** Pooling ranks each leg by its raw backend
  score, which assumes every store queried together reports that leg's scores on the same scale. Two
  ways that assumption is imperfect:
  - **BM25** scores are corpus-relative (per-store IDF and average document length), so a pooled
    BM25 ranking compares numbers that are not strictly commensurable even when every store runs the
    same backend. Cross-store ordering within that leg is therefore approximate.
  - **Dense** scores land in `[0, 1]`, but not via one common mapping. `store-libsql` converts
    distance to score two ways, chosen per store by the encoding its embedder produced: `1 - d/2`
    from a continuous cosine distance (`VectorEncoding::Float32`), and `1 - d/nbits` from a
    sign-only binarized Hamming distance (`VectorEncoding::Binary`) — the latter being what the
    default `pplx-embed-context-v1-0.6b` emits. Same range, different distributions, so pooling
    across a Binary-encoded and a Float32-encoded store would favor whichever mapping runs hotter
    rather than whichever store is more relevant. The two shipped models differ in dimensionality
    (1024 vs 384) so one query cannot currently reach both, but nothing enforces that. Separately,
    `SearchResult.score` documents the leg as "cosine/dot-product": an unbounded dot-product would
    swamp a bounded score outright, and is in any case wrong for the default model, whose vectors
    are unnormalized and which documents cosine as required. Dense scores must be a bounded
    similarity in `[0, 1]` — a precondition of pooling, not a free choice.

  Global pooling is still strictly better than per-store RRF, which gave _every_ store's local
  rank-0 chunk an identical score regardless of quality. But multi-store relevance is not fully
  solved until both legs are calibrated. Tracked by issue #40; see also §"Rejected" above on score
  interpolation.

- **Result shaping:** top-N (default 10) → Citation objects
  ([02-domain-model.md](02-domain-model.md) §6), with per-leg scores retained for debugging
  (`score: {fused, dense, bm25}`). Citations carry a **block reference** and chunk position within
  that block, not just a Markdown span.
- **Reranking: explicitly post-MVP** ([06-roadmap.md](06-roadmap.md) §5). The pipeline leaves a seam
  (rerank stage between fuse and shape) but ships nothing.
- Query rewriting and answer generation are **not** backend-core concerns — they belong to
  downstream consumers (agents, future UI). URL/image as _query_ modes: out of scope v1.

### Context expansion

Context expansion is a first-class retrieval capability, available after initial ranking:

- **Neighboring chunks in the same block:** retrieve the chunks immediately before and after a
  result chunk within the same block, to provide sentence-level continuity.
- **Nearby blocks in the same resource:** retrieve adjacent blocks (by block tree position) in the
  same resource, for section-level context.
- **Full resource block sequence:** retrieve all blocks from a resource in order, for document-level
  context (e.g. for a summarization or answer-synthesis consumer).

Context expansion is exposed as explicit query operations, not applied automatically to search
results.

### Dense search (DiskANN / libsql)

**Decision:** the store backend is libsql (Turso's SQLite fork) with built-in vector search. Dense
vectors are stored as `F32_BLOB` (float32) or `F1BIT_BLOB` (binary) column types, with DiskANN
indexing via `libsql_vector_idx`.

- **Float32 path:** embedding column is `F32_BLOB(dim)`. Search via
  `vector_top_k(table, col, query_blob, k)` which uses the DiskANN index automatically. Score
  conversion: cosine distance → score via `1.0 - distance / 2.0 ∈ [0, 1]`.
- **Binary path:** when the embedder's `vector_encoding()` returns `Binary`, the store writes an
  `F1BIT_BLOB(dim)` column. Binarization: `bit = (x ≥ 0.0)`, packed MSB-first (dim 0 → bit 7 of byte
  0). A 1024-dim float vector becomes 128 bytes. Search uses Hamming distance. Score formula:
  `1.0 − hamming_dist / nbits ∈ [0, 1]`.
- **Index maintenance:** DiskANN indexes are auto-maintained by libsql — no manual
  `create_vector_index` calls are needed. The index is created implicitly when `vector_top_k` is
  first used.
- **BM25 via FTS5:** full-text search uses libsql's FTS5 extension with `bm25()` scoring. FTS5
  indexes are auto-maintained — no manual `create_fts_index` calls. The FTS5 virtual table is
  created alongside the chunks table and kept in sync via triggers.
- **Supported embedders:** pplx local-ONNX models (`pplx-embed-context-v1-0.6b`,
  `pplx-embed-v1-0.6b`) override `vector_encoding()` to return `Binary`. `FakeEmbedder` keeps
  `Float32`.
- **Expected recall drop (binary):** ~2–4 pts on MTEB-ML vs float32 at 1024 dim; cushioned by the
  BM25+RRF hybrid. Future rerank via an int8 copy can recover the gap.

#### Index tuning and the per-chunk cost model

libsql stores every DiskANN node as a **fixed-size** blob, allocated with
`sqlite3_bind_zeroblob(..., nBlockSize)` regardless of the node's actual degree. So the index has an
exact, unavoidable per-chunk cost:

```
block_size = (node_vec_size + 16) + max_neighbors × (edge_vec_size + 16)
```

`max_neighbors` multiplies the edge width, which makes `compress_neighbors` the single largest lever
on total database size. Hence the invariant:

> **`compress_neighbors` must never be wider than the embedding column's own encoding.** It is a
> _compression_ only relative to a wider node vector. Against a narrower one it is an inflation that
> buys no recall.

Violating that invariant is what produced issue #179 (45 GB for ~600k chunks whose raw vectors are
under 1 GB) and issue #177. `compress_neighbors=float8` was chosen in PR #92 while a float32 column
was in play, and was not revisited when the binary column landed. On an `F1BIT_BLOB` column libsql
converts each source bit to `±1` and quantizes _that_ to a byte, so every one of those 1032 edge
bytes held only 0 or 255 — 8× the space for exactly the information the 128-byte node vector already
carried, and therefore zero recall benefit. Pinning `max_neighbors=64` compounded it by overriding
libsql's own guard rail, which caps edge overhead at 50× node overhead.

Measured per-chunk cost at 1024 dimensions (read back from `libsql_vector_meta_shadow`, not derived
— see `store-libsql/tests/vector_index_cost.rs`):

| Column       | `compress_neighbors` | `max_neighbors`        | Bytes/chunk | 600k chunks |
| ------------ | -------------------- | ---------------------- | ----------- | ----------- |
| `F1BIT_BLOB` | _(none — schema v6)_ | _(libsql default: 51)_ | **7,488**   | **4.5 GB**  |
| `F1BIT_BLOB` | `float8` (schema v5) | 64                     | 67,216      | 40.3 GB     |
| `F32_BLOB`   | `float8`             | 64                     | 71,184      | 42.7 GB     |
| `F32_BLOB`   | _(none)_             | _(libsql default: 51)_ | 213,824     | 128 GB      |

The tuning is therefore **encoding-dependent**, and derived in exactly one place (`store-libsql`'s
`vectors::vector_index_params`): binary columns pass `metric=cosine` alone, while float32 columns
keep both parameters, because for a 4 KiB node vector `float8` edges are a genuine 4× compression
and the bare default would be 3× _worse_.

**Recall.** Dropping `compress_neighbors` is lossless by construction: libsql converts a 1-bit
source to float8 by mapping each bit to `±1` and quantizing, giving exactly byte 0 or 255, so the
float8 cosine distance is an exact zero-intercept linear rescaling of Hamming distance
(`F8_distance = (2/dims) × Hamming`). RobustPrune's keep/drop decision is a _ratio_ between two
distances computed the same way, so it is invariant to the substitution. Dropping `max_neighbors` is
a real change — degree 64 → 51, with no compensating change to the search beam
(`insert_l`/`search_l` are fixed constants, unrelated to `max_neighbors`) — so it was measured, not
argued: 6,000 clustered 1024-bit vectors, 200 held-out queries, recall@10 against exact brute-force
ground truth, **6 independent index builds per configuration**:

| Configuration     | mean recall@10 | sd     | min    | max    |
| ----------------- | -------------- | ------ | ------ | ------ |
| v5 (`float8`, 64) | 0.5714         | 0.1164 | 0.3190 | 0.6625 |
| v6 (1-bit, 51)    | 0.5792         | 0.0609 | 0.5085 | 0.6695 |

The difference in means is 0.15 standard errors — no detectable regression, and v6 is nominally
better with half the variance.

> **Benchmarking DiskANN here requires repeated builds.** `diskAnnSelectRandomShadowRow` picks the
> graph traversal's entry point with SQL `RANDOM()`, and it is called _per inserted row_ and _per
> query_. Index construction is therefore nondeterministic, and its quality is bimodal: across the
> 12 builds above, most landed near 0.6 but one v5 build landed at 0.32. A single before/after
> comparison samples that distribution once and can show a 2× swing in either direction — an earlier
> single-run pass of exactly this benchmark appeared to show a 32% regression that repeated trials
> showed did not exist. Never gate a tuning change on one build.

Schema v6 (`shrink_vector_index`) rebuilds an existing binary index into the new shape. The rebuild
reads `chunks.embedding` directly — **no re-embedding and no model download** — but it is a DiskANN
insert per chunk, so it is a long operation on a large store. The freed pages land on SQLite's
freelist, so the file does not shrink until `localdb db vacuum` runs; this is why the `VACUUM`
attempted in #177, _before_ any rebuild had freed anything, correctly reclaimed nothing.
