# Spec 02 — Canonical Domain Model

> Status: accepted draft, revised 2026-08-11. All entities live in the `core` crate and are shared
> by every surface. Field lists are normative for meaning, not for exact Rust types.
>
> **Supersedes:** the Markdown-native IR model (commit `3da56d0`). The block model is reintroduced
> as the canonical intermediate representation — see
> [07-adr-blocks-canonical-ir.md](07-adr-blocks-canonical-ir.md) for the decision record.

## 1. Entity overview

```
Store 1──* Source 1──* Resource 1──* Block 1──* Chunk
                           │                       │
                      IndexJob            Citation (view over Chunk + Resource)
```

Ingestors produce **Resources** containing ordered **Blocks**. Each block has a `BlockKind`,
canonical text, and optional source-location metadata. The chunker operates on blocks (not a
Markdown string), and `heading_path` is derived from the block tree (heading blocks preceding
content blocks). Chunks never cross block boundaries, with one explicit exception: message-window
chunks span multiple `Message`/`Segment` blocks.

## 2. Entities

### Store

A named knowledge base. Unit of sharing, ACLs, indexing policy, and federation.

| Field        | Notes                                                                                                                           |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------- |
| `id`         | Stable ULID, minted at creation; never reused.                                                                                  |
| `name`       | Human-readable, unique per instance.                                                                                            |
| `visibility` | `private` \| `shared`. MVP: only `private` functional; field exists from day one ([01-architecture.md](01-architecture.md) §5). |
| `backend`    | Backend kind + connection info; default `libsql`.                                                                               |
| `indexing`   | Indexing policy: `{chunking, embedding, parsers}` as one unit ([03-config.md](03-config.md) §2).                                |
| `acl`        | Reserved; empty in MVP.                                                                                                         |

### Source

Where a store's content comes from. Each source is driven by an **ingestor** that knows how to
acquire and structure its content.

| Field                | Notes                                                                                                                                                                                                                              |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`                 | ULID.                                                                                                                                                                                                                              |
| `store_id`           | Owning store.                                                                                                                                                                                                                      |
| `ingestor_kind`      | Which ingestor drives this source: `file`, `url`, and future connectors (`notion`, `telegram`, `signal`, `hackmd`, `email`, `transcription`, `feed`). See [01-architecture.md](01-architecture.md) §1 for the `IngestorKind` enum. |
| `spec`               | Kind-specific configuration: root path + globs, URL + refresh interval, API token reference, etc. Stored as JSON; validated by the ingestor's `IngestorConfig`.                                                                    |
| `config_json`        | Ingestor-specific configuration fields (typed per ingestor).                                                                                                                                                                       |
| `source_kind_preset` | Which indexing preset applies (`prose`, `messages`, `code`) — see [03-config.md](03-config.md) §2.                                                                                                                                 |

**Runtime representation:** `SourceRow` in `core::backend` is the concrete Rust type for sources
persisted in the unified database (`localdb.db`). Source CRUD is exposed via `StoreBackend` methods
(`upsert_source`, `delete_source`, `list_sources`, `get_source`, `find_source_by_root_or_url`).

### Resource

One logical content unit produced by an ingestor. Replaces the former `Document` entity. A resource
is: a file, a fetched page, a Notion page, a conversation thread, a transcript, a feed entry.

| Field                       | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `id`                        | **Content-addressed**: `blake3(uri ‖ content_hash)` — see §3.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `source_id`, `store_id`     | Ownership.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `ingestor_kind`             | Which ingestor produced this resource (denormalized from source for queries).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `resource_kind`             | `document` \| `conversation` \| `transcription`. Determines block ordering semantics.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `uri`                       | `Uri` newtype wrapping `url::Url`. Canonical locator (absolute path as `file://`, URL, or connector-defined scheme like `notion://`, `telegram://`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `external_id`               | Arbitrary source-system ID (Notion page ID, Telegram message ID, email Message-ID). Optional.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `external_etag`             | Change detection token from the source system (HTTP ETag, Notion `last_edited_time`, file mtime). Optional.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `external_last_modified`    | The raw HTTP `Last-Modified` conditional-GET validator: stored verbatim as the origin sent it, replayed byte-exact in `If-Modified-Since`. Optional. **Never promoted to any date axis** — in particular, never confused with `modified_at`. The "Date axes (normative)" table below names a _future_ HTTP `Last-Modified` as an eventual source for `modified_at`; those are two different things even once that lands: `modified_at` is a parsed, source-claimed change time on a normative axis, `external_last_modified` is an opaque transport token. Also deliberately **not** an input to `core::ids::compute_metadata_hash` (unlike `external_etag`, which is — see `core/src/ids.rs`): it is a coarser, redundant signal that would add a second rotating-value churn risk for no extra detection power.                                                                                                                                                                                |
| `last_checked_at`           | Our clock: when the liveness sweep last **attempted** a probe of this resource. Advanced on every probe outcome that does not delete the resource — a blocked or unreachable one included, where it is the only thing that moves — so the sweep's oldest-first ordering rotates fairly instead of re-probing a stuck set forever. A probe writes no content and no metadata; the column exists only to throttle re-probing. **Deliberately not one of the four date axes** and not `DateAxis`-filterable — see the note under the date-axes table in §2. A **separate column, never a reuse of `index_updated_at`**: `index_updated_at` normatively means "when we last wrote this resource's stored state" and is exposed publicly as `DocumentInfo.index_updated_at` through `localdb document get`, `GET /v1/documents/{id}`, and MCP `get_document`/`list_documents`; a liveness probe writes nothing, so bumping that column would report a document as re-written when it was only pinged. |
| `content_hash`              | blake3 of ordered block canonical texts concatenated. Drives incremental re-index. Not dependent on Markdown rendering. Alongside `policy_version`, compared against a third, unstored value — `metadata_hash` (`core::ids::compute_metadata_hash`, hashing post-backfill `metadata` + `external_id`/`external_etag`/`modified_at`) — to decide the skip-check's three outcomes: skip, metadata-only update, or full reindex. `modified_at` is `Option<String>`; a no-claim source hashes a stable `None` on every run, so it never churns the skip-check on its own — only a genuine claim change does, which correctly routes to a metadata-only update. See [04-search-pipeline.md](04-search-pipeline.md) §1's "Metadata-only update".                                                                                                                                                                                                                                                       |
| `title`, `mime`, `language` | From extraction. `language` is BCP 47.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `date_original`             | Dublin Core date string (may be partial, e.g. `2026` or `2026-06`). Populated from `dc:date` per source format (coverage varies — see §7); population follows the "Document date" row of the date-axes table below. Never derived from `added_at`, `modified_at`, or `index_updated_at`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `date_parsed`               | Best-effort ISO 8601 parse of `date_original` (sortable). Same source and population rule as `date_original`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `added_at`                  | When this resource _version_ was first indexed — our clock (`now()` at ingestion), never a source value. Because `id` is content-addressed (`blake3(uri ‖ content_hash)`, §3), a content change mints a new resource ID and therefore a fresh `added_at`; a policy-only re-index (chunking/embedding config change, content unchanged) preserves the existing `added_at` — when replacing the same resource ID, the write path leaves the stored row in place and upserts it, and the upsert never touches `added_at` (§9).                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `modified_at`               | The **source-claimed** change time: when the source system says this content last changed (file mtime, feed `updated`\|`published`, future HTTP `Last-Modified`). A claim, not our observation, and never promoted into `date_original`/`date_parsed`. `Option<String>`; absent (`None`) when the source gives no signal — a bare URL fetch, a dateless feed entry, an unreadable file mtime. Never our clock — no exceptions.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `index_updated_at`          | When we last wrote this resource's stored state — full re-index or metadata-only update — our clock (§9).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `thread_id`                 | Conversation thread identifier (conversation resources only).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `channel`                   | Channel/folder/chat name (conversation resources only).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `participants`              | JSON array of participant names/IDs (conversation resources only).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `metadata`                  | `Metadata` enum — see §7. Contains Dublin Core base fields plus resource-kind-specific fields.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `provenance`                | See §4.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `extractor_version`         | Version string of the parser/ingestor that produced the blocks. Enables reprocessing when extraction logic improves.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |

### Date axes (normative)

A resource carries **four** distinct date/time signals. They answer different questions, are set by
different code paths, and must never be conflated or substituted for one another. This table is
normative: any code that writes one axis's field from another axis's source is a bug against this
spec, not an acceptable implementation choice.

**Canonical timestamp form (normative).** Every stored timestamp on the "our clock" axes
(`added_at`, `index_updated_at`, and any `modified_at`/`date_original` value an ingestor derives
from its own RFC 3339 formatting rather than passing a source string through unchanged) is exactly
`YYYY-MM-DDTHH:MM:SSZ` — no fractional seconds, and a literal `Z`, never a numeric `+00:00` offset.
These strings are compared lexicographically, both in Rust and in SQL, so the exact form is a
data-compatibility contract: a stray fractional component or a `+00:00` suffix breaks sort order
against every other stored row.

| Axis                      | Meaning                                                                                    | Field / column                 | Set by                                                                                                                                                                                                   | Never receives                                                                                                                 |
| ------------------------- | ------------------------------------------------------------------------------------------ | ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Document date             | The date the document is _about_, or was authored/adopted — Dublin Core.                   | `date_original`, `date_parsed` | Parsers and connector metadata enrichment carrying an explicit authorship/publication claim — embedded `dc:date`, or a feed entry's `published` (falling back to `updated`) per the feed contract below. | File mtime, HTTP `Last-Modified`, or any other change-detection timestamp that carries no authorship/publication claim.        |
| Index added               | When _we_ first indexed this resource version.                                             | `added_at`                     | Our clock (`now()` at ingestion).                                                                                                                                                                        | Any source-provided value.                                                                                                     |
| Index updated             | When _we_ last wrote this resource's stored state (full re-index or metadata-only update). | `index_updated_at`             | Our clock (`now()` at write). Bumps on every resource-row write, including a metadata-only update that leaves content and `added_at` untouched.                                                          | Any source-provided value.                                                                                                     |
| Source-claimed changed-at | The source system's own claim about when its content last changed.                         | `modified_at`                  | The source: file mtime, feed `updated`\|`published`, future HTTP `Last-Modified`.                                                                                                                        | Never promoted into `date_original`/`date_parsed`; never read back as an index-side timestamp (`added_at`/`index_updated_at`). |

**`last_checked_at` is deliberately not a fifth axis.** The table has four rows and
`core::store::DateAxis` has four variants; `resources.last_checked_at` — the liveness sweep's probe
clock (§1) — is neither of those, and is not `DateAxis`-filterable. It is a separate column for
exactly the conflation hazard this table exists to teach against. It sits adjacent in kind to
`index_updated_at` (both are our clock; both move on a run that touched the resource) and differs on
the one point that matters: a liveness probe writes no content and no metadata, so folding it into
`index_updated_at` would report a document as re-written when it was only pinged. Keeping it off
`DateAxis` is that same judgement one layer up — it is probe-throttle bookkeeping, not a claim about
the document, and it is `NULL` for every resource no probe has reached — everything outside a feed
source, and every feed entry a `--delete` run has not yet picked as a candidate. Under the `NULL`
rule in "Filtering" below, a filter on it would silently exclude nearly every document in a mixed
store. Revisiting it needs two things that are not true today: semantics that have stopped moving,
and a demonstrated operator need that the `--json` index summary's `feed_entries_liveness_checked`
counter does not already meet.

Two rules follow directly from the "Never receives" column and hold across every ingestor, present
and future:

1. **No ingestor may write a change-detection timestamp into `dc:date`.** File mtime and HTTP
   `Last-Modified` are instances of the "source-claimed changed-at" axis, not the "document date"
   axis — neither may populate `date_original`/`date_parsed`, no matter how plausible a stand-in
   they seem when the source supplies no explicit `dc:date`. A feed entry's `published` (or
   `updated` when `published` is absent) is different in kind: it is an explicit publication claim,
   and the feed contract below assigns it to `dc:date` deliberately — while `modified_at` takes
   `updated`|`published` (the opposite preference), keeping the two axes distinct even when both
   draw on feed fields.
2. **No parser-derived `dc:date` may be read back as an index timestamp.** `date_original` and
   `date_parsed` never substitute for `added_at`, `index_updated_at`, or `modified_at` — a document
   whose Dublin Core date is missing or unparsed stays missing on those fields; it is never
   backfilled from an index-side clock or vice versa.

**Filtering (normative).** Every axis is filterable via
`MetadataFilter::DateAfter`/`DateBefore { axis: DateAxis, value }` (`core::store::DateAxis`), one
inclusive bound per call: `DateAfter` is `>=`, `DateBefore` is `<=`. `DateAxis`'s four variants have
public names distinct from the column names above — `added` (Index added), `updated` (Index
updated), `modified` (Source-claimed changed-at), `document` (Document date) — deliberately, so a
filter's public name never leaks storage detail. A `None`/`NULL` axis value (the nullable
`modified_at`/`date_parsed` fields) fails **every** bound in **both** directions — the single most
surprising behavior of this feature: a document with no claimed `modified_at` is never returned by a
`modified`-axis filter at any bound, not just an unsatisfiable one. `DateAxis::Updated` is
SQL-pushdown-only: it filters correctly against the real backend, but has no Rust-side round-trip on
a `ChunkRecord`, because the store always stamps `index_updated_at` with its own write-time clock
rather than accepting a caller-supplied value (see the "Index updated" row above).

`DateBefore` widens to the latest instant consistent with a value's own precision
(`core::dates::widen_date_upper_bound`) before comparing, because a short prefix always sorts less
than a longer string it prefixes. The two operands widen under different rules, since they become
partial for different reasons:

- **The bound is always widened, on every axis.** It is whatever the caller supplied, so it can be
  partial regardless of what the axis stores. Without widening, an inclusive `added_before: "2026"`
  would exclude every resource added during 2026 — `"2026-06-10T12:00:00Z" <= "2026"` is false.
- **The stored value is widened only on the `document` axis**, the only one whose column can hold a
  partial value (`date_parsed` is normalized to a bare `"YYYY"`, `"YYYY-MM"`, or full `"YYYY-MM-DD"`
  — see `core::dates::parse_partial_iso8601`). `added`, `updated` and `modified` always hold
  full-width RFC 3339 per the canonical form above, so widening them would be a no-op and is
  skipped.

`DateAfter` needs no widening in either operand: a short prefix already sorts below any longer bound
it cannot confirm, which is the correct conservative reading.

The widening is calendar-unaware — `"YYYY-MM"` always widens to day 31 regardless of the real month
length, because `"31"` string-compares at or above any real day-of-month and SQLite cannot run
per-row calendar arithmetic without a registered custom function.

### Block

A typed, ordered unit of content within a resource.

| Field           | Notes                                                                                        |
| --------------- | -------------------------------------------------------------------------------------------- |
| `resource_id`   | Parent resource.                                                                             |
| `seq`           | Ordering within the resource (0-indexed). Stable as long as resource content doesn't change. |
| `kind`          | `BlockKind` — see §2a.                                                                       |
| `text`          | Canonical text content of the block. Every block kind has a text representation.             |
| `metadata_json` | Kind-specific structured metadata (e.g. heading level, sender, timestamp).                   |
| `location`      | `BlockLocation` — optional source-location data for citation/navigation (§2b).               |

**Identity:** blocks are identified by `(resource_id, seq)`, not content-addressed. They are derived
content that can be regenerated by re-running the ingestor.

### Feed connector (`SourceSpec::Feed`)

`ingestor_kind = feed` parses RSS 2.0, Atom 1.0, and JSON Feed via `feed-rs`, with `feed-rs`'s
`sanitize` feature (an `ammonia` pass over embedded entry HTML) always applied.

| Field                   | Notes                                                                                                                                                       |
| ----------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `url`                   | Feed URL.                                                                                                                                                   |
| `max_entries`           | `Option<u32>`. Cap on entries considered per fetch, applied after the sort described below. `0` is rejected at config time, not treated as "index nothing." |
| `fetch_full_content`    | `bool`, default `true`. Selects **discovery mode** (`true`) vs. **single-document mode** (`false`) — see below.                                             |
| `refresh_interval_secs` | `Option<u64>`. Same shape and column (`sources.refresh`) as `SourceSpec::Url`.                                                                              |

`{max_entries, fetch_full_content}` persist as JSON in `sources.config_json` — the column exists at
baseline (v4), so the feed connector needed no schema migration. `refresh_interval_secs` persists in
the pre-existing `sources.refresh` column alongside `url` sources.

**Discovery mode (default).** A feed is treated as a URL-discovery meta-wrapper, not itself the
indexed content: parse the feed, then for each entry resolve the entry's link and run it through the
_same_ per-URL pipeline `UrlIngestor` uses for `url` sources (fetch page → parser chain → blocks →
`Resource`), via a shared `ingest/src/url_pipeline.rs` helper — a feed entry and a
directly-configured `url` source produce identically-shaped Resources. Entry metadata enriches the
resulting Resource: `external_id` = the entry's feed-native ID, `creator` = entry authors — falling
back to the **feed-level** `<author>` when the entry declares none, per Atom's inheritance rule (RFC
4287 §4.2.1); an entry's own authors win outright and the two lists are never merged — the metadata
date = the entry's published/updated timestamp, `metadata.source` = the feed URL (provenance back to
the discovering feed), `external_etag` captured from the entry-link fetch. The Resource's `uri` (and
therefore its content-addressed `id`, §3) keys off the entry's **pre-redirect, feed-declared link**,
not wherever it 30x's to, so re-fetching the same feed resolves to the same Resource identity
regardless of redirect-target churn. That is a statement about _identity_, distinct from _link
resolution_: a relative entry link (e.g. `<link>article.html</link>`) is resolved by `feed-rs`
against the feed's **effective (post-redirect) URL**, not the configured `feed_url` — a feed that
301's to a new host must still resolve its entries' relative links against that new host, not the
stale one it was configured with. `xml:base`, where present in the feed XML, still takes precedence
over that base URI (feed-rs's own resolution rule). Once resolved, the link is the feed-declared
link that identity keys off, per the paragraph above — resolution decides _where a relative link
points_; identity decides _what URI names the Resource_, and stays pinned to that resolved link
regardless of where the linked page itself later redirects. Entries with no link get a fragment URI
instead: `{feed_url}#entry:{entry.id}`.

**General connector pattern.** Discovery mode plus the fragment-URI fallback is not feed-specific —
it's the expected shape for any ingestor that discovers sub-resource URIs from a parent resource:
enrich the discovered Resource from the parent's metadata, key its identity off the discovered URI,
and fall back to `{parent_uri}#fragment:{id}` when no addressable URI exists. Two connectors on the
roadmap ([06-roadmap.md](06-roadmap.md)) are expected to follow it: email (#114, discovering
message/attachment URIs from a mailbox) and conversation exports (#129, discovering per-message
permalinks from a thread).

**Single-document mode** (`fetch_full_content: false`): the whole feed becomes **one** Resource,
`uri` = the feed URL itself, assembled deterministically rather than fetched:

```
# {feed.title | "Untitled Feed"}

{feed.description, if present}

## {entry.title | "Untitled Entry"}

*By {authors} — {date RFC3339} — {link}*

{entry body: content, else summary, else nothing}

## {next entry.title | "Untitled Entry"}
...
```

The byline line omits missing parts outright — no placeholder text, and the entry's guid never
appears in it. `{authors}` follows the same feed-level inheritance rule discovery mode uses: an
entry with no `<author>` of its own bylines the feed's.

**Destination policy (entry links).** Entry links are the only locators in localdb chosen by a third
party rather than by the operator, so discovery mode fetches them through a
**public-destination-only** HTTP client. Any request — or any redirect hop — whose host is, or
resolves to, a non-globally-routable address (loopback, RFC 1918 private, link-local incl.
`169.254.169.254`, CGNAT, ULA, multicast, reserved, and their IPv4-mapped-IPv6 forms) is refused
before a connection is opened, and the entry falls back to its embedded content exactly like
`Gone`/`Unsupported`/`Empty` below. Filtering happens inside a custom DNS resolver, so the address
reqwest connects to is the address that was checked (no rebinding window). **The feed URL itself and
`url` sources are unaffected** — both are operator-configured, and a homelab or LAN address is a
legitimate thing for an operator to point localdb at. Guarding entry links only also means an
internal feed degrades gracefully rather than failing at step one: its entries are still indexed,
from their embedded summaries. There is no opt-out in v0.1; the per-source/global allowance for
private destinations is tracked as a known gap in
[../docs/architecture.md](../docs/architecture.md#known-gaps).

**Both modes:** entries are stable-sorted by `published.or(updated)` descending (entries with
neither date sort last, stable among themselves), then **deduplicated by resolved resource URI** —
the same URI that becomes the Resource's identity (the entry's resolved link, or the
`{feed_url}#entry:{entry.id}` fragment for a link-less entry) — keeping the first (i.e. newest)
survivor and dropping the rest, then truncated to `max_entries`. `max_entries` therefore counts
_distinct_ resource URIs, not raw entry count: two entries that resolve to the same URI cost one
slot, not two. First-wins (not last-wins) is deliberate: when a feed lists the same URI more than
once, the newest listing is the feed's most current claim about that URI, so it is the one that
should win whichever content ends up indexed for it.

**Timestamps.** Feed-produced Resources map times as follows. `added_at` is always ingestion-time
`now()` — it records when _our store_ first saw the resource, never a feed-claimed date.
`modified_at` comes from the feed when it says anything: per entry `updated.or(published)`
(discovery mode and the embedded fallback), and in single-document mode `feed.updated`, else the
newest entry's date, else absent (`None`) — never `now()`. Creation/publication stays in
`dublin_core.date` = `published.or(updated)` (the conventional DC slot) — note the opposite
preference order from `modified_at`, matching each field's semantics. Like all enrichment, an
already-indexed entry whose content hash is unchanged does not retroactively pick these up (the
pipeline's incremental-skip runs before any store write).

Both `modified_at` and `dublin_core.date` are formatted in the canonical `…Z` form described above,
not the numeric `+00:00` offset `chrono`'s default `DateTime::to_rfc3339()` would otherwise emit.

**Fallback and error handling:**

- Discovery mode, entry-link fetch returns `Gone`/`Unsupported`/`Blocked`: falls back to indexing
  the entry's own embedded content/summary at the same URI, instead of dropping the entry. All three
  are _stable_ properties of the link (a 404 stays a 404; a refused destination is refused
  identically next run), which is what makes falling back safe — contrast the transient cases below.
- Discovery mode, entry-link fetch returns a transient `FetchError`/`ParseFailed`: no fallback —
  reported as a per-item error and skipped this run. Falling back here would flip the Resource's
  content hash between "full page" and "feed summary" on every transient outage, forcing needless
  re-embedding; the existing good index is left alone instead.
- A fetched entry page — or, having fallen back that far, the entry's own embedded content — that
  extracts to empty or whitespace-only Markdown is **unusable, not empty**: it falls through the
  same `content → summary → title` chain that `Gone`/`Unsupported` use, and never yields a
  zero-block Resource. A zero-block Resource reaching `index_resource` is refused by the sink and
  reported as a skip — it can no longer delete the previously indexed document (see
  [04-search-pipeline.md](04-search-pipeline.md) §1) — but the fallback chain's rationale is
  unchanged: an entry whose page yields nothing should still be indexed from its summary or title
  rather than left to the sink's refusal, which preserves the _old_ content rather than producing
  the best content available now. The embedded-content chain's own empty-Markdown guards
  (`feed_ingestor.rs`'s `entry_routed_content`) exist for that reason, extended here to the
  fetched-page path that shares the same fallthrough logic.
- An invalid feed URL in config fails the whole source run fast (`invalid_config`). Everything else
  data-driven — malformed feed XML, malformed entries, entry-link fetch failures — is per-item
  `on_skipped(Error)` + continue. An empty feed (zero entries) is valid, not an error.
- Feed autodiscovery from HTML pages (`<link rel="alternate">`) is out of scope.

**Retention:** feed sources are exempt from the pipeline's delete-sweep — a feed exposes only its
most recent entries, so an entry falling out of the feed does not mean it was deleted upstream, and
a feed `304`/transient-empty-parse would otherwise wipe an entire source's index in one sweep.
`source remove` still cascades normally. See
[docs/architecture.md#known-gaps](../docs/architecture.md#known-gaps) for the resulting archive
semantics and the pruning follow-up.

**Ordering semantics** depend on `ResourceKind`:

- `document` — logical reading order
- `conversation` — chronological message order
- `transcription` — transcript time order

**Conditional GET and pruning.** Feed-document validators (`sources.feed_etag`,
`sources.feed_last_modified`) live on the **`sources` row**, not on a `resources` row. In discovery
mode the feed document itself never becomes a `Resource` at all, so there is nowhere else to put
them; a phantom zero-chunk `resources` row for the feed document was considered and rejected —
`store-libsql/src/registry/documents.rs` queries `resources` with no chunk join, so such a row would
leak straight into `list_documents`/`count_documents`. Entry-link validators, by contrast, live on
the entry's own `resources` row like any other URL-fetched resource — no special-casing there.
Editing a feed source's URL must null every one of these `sources` cache columns — both validators
and the input digest below: a stored validator is only meaningful against the origin that issued it,
and a changed feed URL is a new origin.

> **Local-input gate (normative).** The RFC 9110 contract a conditional GET rests on binds the
> _origin_ only: a compliant origin changes its validator whenever its own representation changes.
> It knows nothing about our side of the pipeline, so a change to how _we_ would process the very
> same bytes must not be allowed to hide behind a 304. Before the stored feed-document validators
> are replayed, the ingestion layer compares a digest of the local inputs that determine what an
> unchanged feed would produce — `policy_version`, `fetch_full_content`, and `max_entries` — against
> the digest stored on the source row (`sources.feed_inputs_digest`). On mismatch the validators are
> treated as absent for that run: the feed document is fetched unconditionally, the entry loop runs,
> and every entry is reprocessed under the new inputs. This is the same rule, one layer up, as the
> resource-level suppression that gates entry-link conditional headers on `policy_version`
> ([04-search-pipeline.md](04-search-pipeline.md) §1), and shares its role as a designated join
> point: any future local input that can change a feed run's output without changing the feed XML
> joins this digest.
>
> The digest is written **with** the validators, in the same update, and only when a fetch actually
> produced them. A run that errored, was blocked, or never fetched the document leaves both columns
> exactly as it found them. Writing the digest on its own would create the failure it exists to
> prevent: it would declare the _stored_ validators — still the ones captured under the old inputs —
> trustworthy under the new ones, and the next run would replay them, take the 304, and skip the
> very entry loop the input change was supposed to force. Leaving the pair untouched keeps the
> mismatch standing until a run under the new inputs actually succeeds. A row whose digest is `NULL`
> — every row predating the column — counts as a mismatch, so the first run after an upgrade fetches
> unconditionally: a validator captured before this gate existed carries no evidence about which
> inputs produced it.
>
> **The gate forces reprocessing, not replacement (normative, and a known gap).** A digest mismatch
> makes the next run read the feed again under the new inputs. It does **not** reconcile what the
> _old_ inputs left in the store, and a `fetch_full_content` flip changes which resources a feed
> produces at all — so both representations end up indexed, in either direction:
>
> - **discovery → single-document.** The feed document becomes one resource under the feed URL;
>   every entry resource the source indexed before stays, because feed sources are exempt from the
>   presumed-gone delete-sweep ("Retention" above) and no entry callback fires for them any more.
>   The entries are then searchable twice — once on their own, once inside the single document's
>   body.
> - **single-document → discovery.** The old feed-root resource stays, and nothing can reclaim it:
>   it is the one feed resource carrying no `external_id`, which is exactly the predicate the
>   liveness sweep excludes on ([04-search-pipeline.md](04-search-pipeline.md) §1, "The feed's own
>   document is never a candidate").
>
> **A URL edit is the same unreconciled transition (normative, and the same gap).** Nulling the
> cache columns above is all a URL edit does. Retargeting a source from feed A to feed B therefore
> makes the next run read B from scratch and leaves every resource A produced exactly where it is,
> and nothing reclaims them: feed sources are exempt from the presumed-gone delete-sweep
> ("Retention" above), so absence prunes nothing; the liveness sweep probes them — a URL edit makes
> every one of A's entries permanently unobserved — but deletes only on a confirmed 404/410, so an
> old entry link that still serves `200` stays indexed forever; and if A was indexed in
> single-document mode, its feed-root resource carries no `external_id` and is excluded from the
> sweep outright. Both feeds are then mixed in one source's results permanently. This trigger is the
> more ordinary of the two — editing a URL is routine config maintenance, where flipping
> `fetch_full_content` is a deliberate change of indexing strategy — and the more damaging, because
> A and B are unrelated content rather than two representations of the same feed.
>
> None of the three is reconciled today; `source remove` is the only thing that clears any of them.
> [#319](https://github.com/dokterbob/localdb/issues/319) carries the fix. It is a gap rather than
> an oversight in this gate: every delete this system performs is justified by evidence about the
> _origin_ — a confirmed 404/410, or an absence from a source that enumerates exhaustively — and
> this one would be justified by a change to **our own config**. That is a different kind of delete,
> and it needs its own answer to a question the deletion model has no place for yet: whether it
> applies without `--delete`. A mode flip or a retarget is a replacement rather than a prune, which
> argues that it should; `--delete` guarding every other delete argues that it should not.
> Reconciling on the wrong side of that question silently drops a source's index on a config edit,
> so the gap is disclosed rather than closed by guess.
>
> **Partial entry passes withhold the validators too (normative).** The same pairing rule extends
> past the fetch to the run as a whole: fresh feed-document validators are persisted only by a run
> that finished with **zero** errors. An entry that failed transiently — its link timed out, its
> index write failed — is not reflected in the store, but the feed XML that lists it is unchanged,
> so storing that XML's validators would let the next run 304 and never retry the entry. The entry
> would stay stranded until the feed document itself changed, which for an aging entry can be
> indefinitely. The cost is disclosed rather than mitigated: while any entry keeps failing, every
> run refetches the feed document in full. That is the cheap half of the work — the expensive half,
> re-fetching and re-indexing unchanged entries, is still avoided by their own resource-level
> validators. The 304 short-circuit on the feed document itself is a deliberate, accepted trade-off,
> not a mitigated one. When the feed document returns 304, the entry loop does not run at all, so
> zero entry callbacks fire that run — drift on an already-indexed entry's own _page_ goes unnoticed
> for as long as the feed XML itself is unchanged. **In discovery mode that run reports no document
> at all (normative):** the document counters partition `docs_seen`, and in discovery mode the feed
> document is not a `Resource` and never becomes one, so reporting it as a skipped document would
> count a document that does not exist — the same leak the rejected phantom `resources` row above
> would have caused, one layer up. In single-document mode the feed document _is_ the resource, so
> there it is reported as an ordinary unchanged skip. The 304 short-circuit is a source-level event
> in one mode and a document-level one in the other, and only the mode decides which. This is
> accepted because it rests on the same contract conditional GET relies on everywhere else in the
> system: a compliant origin changes its validator whenever any part of the representation changes
> (RFC 9110 §8.8.1). No cache-busting forced refresh is provided. What a 304'd run's empty seen-set
> means for the liveness sweep is settled in one place, next to the guard's normative definition:
> [04-search-pipeline.md](04-search-pipeline.md) §1 "Aged-out feed entries: the liveness sweep".

While an entry is still _inside_ the feed's window and a run actually reads that window, a `Gone`
(404/410) result on its link keeps falling back to the feed's own embedded content exactly as today
— the feed is still vouching for that URI, so the link's disappearance from the web is not by itself
a deletion signal. The feed connector's existing exemption from the presumed-gone delete-sweep is
unchanged by any of this: an entry merely scrolling off the window is still never a delete on its
own.

Window membership does **not**, however, protect an entry from the liveness sweep, and this spec
does not claim it does. The sweep's candidates are the entries a run did not observe, and a run
whose feed document answered 304 observes none of them — its seen-set is empty (above), so it
carries no membership information at all, and nothing persists membership to consult instead. Under
`--delete`, a confirmed 404/410 on an entry's own link therefore prunes that entry whether or not
the feed still lists it. Two things bound that, and they are what make it acceptable: `--delete`
itself, without which the sweep issues no requests and deletes nothing, and the requirement for a
positive 404/410 from the entry's own origin — absence is still never, on its own, a delete.

The accepted cost is that those two rules disagree about the same entry. A still-listed entry whose
link answers 404/410 persistently is pruned by a `--delete` run, recreated from the feed's embedded
content on the next run where the feed document changes, and pruned again by the next `--delete` run
— paying a full re-chunk and re-embed on every recreation. Making a `Gone` link stop the fallback
would collapse the two rules into one and let the deletion stick, but it drops entries from
full-text feeds whose links 404 while their XML carries the whole post, and it changes behaviour
that predates conditional GET here; [#323](https://github.com/dokterbob/localdb/issues/323) carries
that decision. Until it lands the trade-off is disclosed, not mitigated.

See [04-search-pipeline.md](04-search-pipeline.md) §1 "Aged-out feed entries: the liveness sweep"
for the candidate rule and the bounds above, and §1 "Deletes" for the surrounding sweep mechanics.

### §2a. BlockKind

| Kind          | Text content               | Metadata fields                                                                                         | Typical sources                       |
| ------------- | -------------------------- | ------------------------------------------------------------------------------------------------------- | ------------------------------------- |
| `Heading`     | Heading text               | `level: u8` (1–6)                                                                                       | Documents, Notion pages               |
| `Text`        | Running body text (coarse) | —                                                                                                       | Documents, HTML, Notion               |
| `Code`        | Code content               | `language: Option<String>`                                                                              | Markdown fences, Notion code blocks   |
| `Table`       | Text rendering of table    | `headers: Vec<String>`, `rows: usize`                                                                   | Documents, spreadsheets               |
| `Message`     | Message body text          | `sender: String`, `timestamp: Option<String>`, `message_id: Option<String>`, `reply_to: Option<String>` | Conversations (chat, email)           |
| `Segment`     | Transcript segment text    | `speaker: Option<String>`, `start_ms: u64`, `end_ms: u64`                                               | Transcriptions (SRT, VTT, Whisper)    |
| `Reference`   | `"[label](target)"`        | `target: String`, `label: Option<String>`, `ref_type: Option<String>`                                   | Wikilinks, Notion mentions, citations |
| `Attachment`  | `"filename: description"`  | `filename: String`, `mime: Option<String>`, `size_bytes: Option<u64>`                                   | Email attachments, Notion files       |
| `Frontmatter` | Raw frontmatter text       | `format: String` (yaml/toml/json)                                                                       | Markdown, Obsidian                    |
| `Image`       | Alt text or OCR text       | `alt: Option<String>`, `src: Option<String>`                                                            | Documents with images                 |

**Coarse `Text` kind:** `Text` is the single running-body-text kind; it folds the former
`Paragraph`/`Quote`/`List` variants. `markdown_to_blocks` emits one `Text` block per run of
consecutive running-text content (paragraphs, lists, blockquotes, HTML) between structural
boundaries (`Heading`/`Table`/`Code`/`Image`/document start-end). Rationale and chunker
consequences: [04-search-pipeline.md](04-search-pipeline.md) §3 and
[07-adr-blocks-canonical-ir.md](07-adr-blocks-canonical-ir.md) ("Ontology axes: kind ⊥ role ⊥
group").

### §2b. BlockLocation

Source-location metadata for citation and navigation. Not all fields apply to every block kind.

| Field                    | Notes                                                              |
| ------------------------ | ------------------------------------------------------------------ |
| `page`                   | Page number (1-indexed, for PDFs and paginated documents).         |
| `bbox`                   | Bounding box `{x, y, width, height}` (for PDFs with layout).       |
| `section`                | Section identifier or path (e.g. `["Chapter 1", "Introduction"]`). |
| `line_start`, `line_end` | Line range in source file (for code and plain text).               |
| `uri_fragment`           | URI fragment (e.g. `#heading-id` for HTML).                        |

### Chunk

The retrieval unit: what gets embedded and indexed.

| Field                     | Notes                                                                                                                                                                                                                                                                                          |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`                      | **Content-addressed**: `blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)` — stable across re-runs over identical content. Computed _after_ `block_seq`/`seq_in_block` are assigned, so both are inputs to the hash, not derived from it.                                            |
| `resource_id`, `store_id` | Ownership.                                                                                                                                                                                                                                                                                     |
| `block_seq`               | Sequence number of the parent block (denormalized, for efficient ordering without a join).                                                                                                                                                                                                     |
| `seq_in_block`            | Chunk position within the block (0-indexed).                                                                                                                                                                                                                                                   |
| `text`                    | Chunk text (also feeds BM25).                                                                                                                                                                                                                                                                  |
| `heading_path`            | Derived from the block tree: heading blocks preceding this content block. JSON array.                                                                                                                                                                                                          |
| `embedding`               | Dense vector (in backend, not in core serialization).                                                                                                                                                                                                                                          |
| `location`                | `ChunkLocation` — refined sub-block position (optional). Persisted as `location_json`: `{"start": N, "end": N, "window_block_seqs": [..]}`, with `window_block_seqs` absent/optional for non-window chunks. `ChunkRecord` carries this as `window_block_seqs: Vec<u32>` (`#[serde(default)]`). |

**Invariant:** a chunk is a subdivision of exactly one block. The canonical block reference is the
triple **`(store_id, resource_id, block_seq)`** — there is no `block_id` column; blocks are looked
up by sequence number, not by a synthetic row reference. The chunks index is
`(store_id, resource_id, block_seq, seq_in_block)`. Chunks never cross block boundaries.

**Why not `block_id`:** an earlier revision of this schema referenced the parent block via
`chunks.block_id` (a `blocks.rowid` foreign key). That column is dropped (#128): rowids are not
stable across a replace (delete+insert of a resource mints new block rows), and window chunks (#129)
need to reference a _set_ of block sequence numbers, which a single scalar foreign key cannot
express. `(store_id, resource_id, block_seq)` is stable and generalizes to sets.

**Span semantics:** Chunk spans (`location.start`, `location.end`) are **block-relative byte
offsets** — they index into the parent block's `text`, not the full document Markdown. Combined with
`block_seq`, they provide a complete location: `(resource_id, block_seq, span)`. Document-relative
offsets are not stored or computed.

For prose and code chunks (`Text`/`Code`-block content), a span locates its chunk's exact text:
`block.text[span.start..span.end] == chunk.text`. **Adjacent spans within a block are not guaranteed
contiguous** — the underlying splitter trims whitespace from chunk boundaries, so a small gap
between one chunk's `end` and the next chunk's `start` is normal, not a bug. Any such gap contains
only whitespace. Spans are therefore **not a partition of the block**: consumers MUST NOT
reconstruct block text by concatenating chunk spans or chunk texts in sequence — use `block.text`
directly if the full block is needed.

Two chunk shapes are exempt from the exact-slice equality; consumers MUST NOT slice `block.text` by
span expecting it to equal `chunk.text` for them:

- **Reconstructed table chunks:** a table chunk's text is normally _reconstructed_ Markdown — the
  header row is re-emitted in every chunk — so no substring of the block corresponds to it. These
  chunks carry the placeholder span `(0, 0)`; their span is not meaningful. Table blocks that fall
  back to the code chunker (a malformed table with no recognizable header/separator row, or a single
  row too large to fit a chunk) emit chunks with real spans that DO satisfy the exact-slice
  contract. Rule of thumb for `block_kind == "table"`: a `(0, 0)` span is a placeholder; a
  non-degenerate span slices exactly.
- **Message chunks:** all `messages`-preset chunk text is prefixed with sender/timestamp metadata
  that is not part of any block's `text`. Sliding-window chunks additionally span multiple
  `Message`/`Segment` blocks — an explicit multi-block chunking mode — and carry the placeholder
  span `(0, 0)`. The `ChunkLocation` carries `window_block_seqs`, the set of participating block
  sequence numbers (`window_block_seqs` is non-empty for every `messages`-preset chunk, including
  the single-oversized-turn split case where it has exactly one element; it is empty for prose,
  code, and table chunks). For an oversized-turn split chunk, the span _is_ meaningful but locates
  the chunk's text within the raw turn text, minus the prepended prefix: `block.text[span]` equals
  `chunk.text` with the sender prefix stripped.

### Citation

Not a stored entity: the **canonical result shape** every surface uses (§6).

### IndexJob

A unit of indexing work with observable state. Fields: `id` (ULID), `store_id`, `scope` (full store
/ one source / one resource — the one-resource variant, `IndexJobScope::Document`, is accepted by
the type but currently unreachable: nothing constructs it, since `POST /v1/jobs` has no
`resource_id` field), `state` (`pending` → `running` → `done` | `failed`), `stats`, `error`,
timestamps. `stats` (`IndexJobStats`) carries `docs_seen`, `docs_indexed`, `docs_skipped` (unchanged
content hash), `docs_metadata_updated` (resource row rewritten in place, no chunks touched),
`docs_deleted`, `docs_prunable` (would-have-been-deleted count under `DeletionPolicy::Retain`),
`feed_entries_liveness_checked` (liveness candidates probed this run — §1 and
[04-search-pipeline.md](04-search-pipeline.md) §1), `chunks_written`, `unsupported_format_count`,
`error_count`, and `sources_count` (size of the job's resolved scope before processing,
distinguishing "nothing to index" from "sources existed but nothing needed indexing"). Both embedded
and daemon-submitted jobs run through the same async engine (`server::job_exec::run_job`, driven by
a `JobQueue`) — the CLI's embedded mode spins up its own in-process, single-job `JobQueue` per
invocation rather than running synchronously outside the job model; the daemon's `JobQueue` is
long-lived and serves every job for the process's lifetime, one at a time per store (a per-store
in-flight guard rejects a second concurrent submission with `index_in_progress`)
([05-surfaces.md](05-surfaces.md) §3).

## 3. ID scheme

**Decision:** entities that exist by fiat (Store, Source, IndexJob) get **ULIDs**; entities derived
from content (Resource, Chunk) get **content-addressed blake3 IDs** as defined above.

**Rationale:** content-addressed IDs are the federation prerequisite — two nodes indexing the same
content derive the same chunk identity, enabling dedup, provenance comparison, and integrity checks
without coordination ([VISION.md](../VISION.md)). They also make re-indexing idempotent.
**Rejected:** auto-increment rows (meaningless off-node); UUIDv4 for resources/chunks (stable only
by table lookup, not by content).

Consequence: a resource edit produces a _new_ resource ID; the pipeline treats it as replace-by-URI
(delete chunks of the old ID, insert new) — see [04-search-pipeline.md](04-search-pipeline.md) §2.

**Block identity:** blocks are identified by `(resource_id, seq)`, not content-addressed. They are
derived content — stable as long as the resource content and extractor version don't change. When
the resource is re-ingested, blocks are replaced entirely.

## 4. Provenance

Every resource and every chunk carries:

| Field          | Notes                                                                          |
| -------------- | ------------------------------------------------------------------------------ |
| `origin_store` | Store ID where it was first indexed (≠ current store after future federation). |
| `source_ref`   | Source ID + ingestor kind.                                                     |
| `fetched_at`   | Acquisition time: the resource's `added_at` (our ingestion clock).             |
| `content_hash` | blake3 of resource content (ordered block texts concatenated).                 |
| `share_path`   | Reserved, empty in MVP: list of (node, store) hops for federated content.      |

**Write path.** A chunk's `fetched_at` is always taken from its resource's `added_at`, never its
`modified_at` — it is persisted as `resources.added_at`, and that is the column
`MetadataFilter::DateAfter`/`DateBefore { axis: DateAxis::Added, .. }` filter on and every
citation's `provenance.fetched_at` reports.

**Surface exposure.** Three separate claims, which are easily conflated and are kept distinct here:

1. `CitationProvenance.fetched_at` (§6) reports exactly one axis — "index added" (`added_at`) — and
   no other. `CitationProvenance` carries no field for `modified_at`, `index_updated_at`, or
   `date_parsed`; those three are absent from every citation, on every surface.
2. `Citation.metadata` (§6, populated from the chunk's own `metadata_json`) already carries the raw
   Dublin Core `dc:date` on every citation, on every surface — `metadata.dublin_core().date` is
   exactly `date_original` before `core::dates::parse_partial_iso8601` normalizes it into
   `date_parsed`. So `date_original` is **not** absent from citations — only `date_parsed` and
   `index_updated_at` are (see point 1). `document get`/`document list` (`DocumentInfo`,
   specs/05-surfaces.md §2-4) are the only surface that returns those two directly.
3. Search filtering is not limited to the "index added" axis: `MetadataFilter::DateAfter`/
   `DateBefore` take a `DateAxis` (`added` | `updated` | `modified` | `document`, §2's "Date axes
   (normative)"), so a query can scope on any of the four, regardless of which axis is (or isn't)
   visible on the resulting citation. A `None`/`NULL` axis value fails every bound in both
   directions, and a `document`-axis `DateBefore` bound is widened to the latest instant its
   precision allows before comparing (§2's "Date axes (normative)" §"Filtering" for both rules in
   full).

## 5. Conversations and non-document resources

The resource model natively supports non-document content shapes:

- **Conversations** (chat, email): `resource_kind = conversation`. Each message is a `Message` block
  with sender, timestamp, and message ID. Thread identity via `thread_id` on the resource. Chunked
  by the `messages` preset (sliding turn windows).
- **Transcriptions** (SRT, VTT, Whisper JSON): `resource_kind = transcription`. Each segment is a
  `Segment` block with speaker, start/end timestamps. Chunked by time windows respecting speaker
  boundaries.
- **Documents** (files, web pages, Notion pages): `resource_kind = document`. Blocks follow logical
  reading order. Chunked by the `prose` or `code` presets dispatched per block kind.

Metadata is resource-kind-specific via the `Metadata` enum (§7), not open key-value `meta` keys.

## 6. Citation model

Every search hit, on every surface, resolves to the same citation structure:

```json
{
  "resource_id": "...",
  "uri": "...",
  "title": "...",
  "block": { "seq": 3, "kind": "text", "page": 12 },
  "chunk_position": { "seq_in_block": 0 },
  "location": {
    "span": { "start": 120, "end": 512 },
    "window_block_seqs": [3, 4, 5]
  }
}
```

That's the shape of the field list distinctive to the block model; the full `Citation` also carries
`chunk_id`, `store: {id, name}`, `heading_path`, `snippet` (chunk text, possibly trimmed), the full
`score: {fused, dense, bm25}` breakdown, `provenance: {fetched_at, content_hash}`, and `metadata`
(the tagged `Metadata` enum — Dublin Core base + resource-kind-specific fields, §7). There is no
top-level `document_id`, `block_seq`, `block_kind`, or `span` — those are superseded by
`resource_id`, the nested `block {seq, kind, page}`, `chunk_position {seq_in_block}`, and
`location {span, window_block_seqs}` respectively. `window_block_seqs` is present only for
message-window chunks (§2); absent otherwise.

`block.page` is the 1-indexed page number for paginated source formats (today: PDF), copied from the
originating block's `location.page` (§2b); absent for non-paginated formats and for chunks indexed
before page plumbing existed. **Page attribution rule:** a block's page is the page containing its
_first contributing byte_ in the extracted Markdown. Blocks are never split at page boundaries — a
paragraph or coarse `Text` run that crosses a page break carries the page it starts on. (Splitting
would fight the coarse-`Text` run packing (#158), which packs chunks within blocks.)

Surface mappings — defined here once, referenced by [05-surfaces.md](05-surfaces.md): **HTTP**
returns the structure verbatim as JSON. **CLI** renders `uri` + heading path + snippet (and full
JSON with `--json`). **MCP** returns it as structured tool output content, never as prose-only text,
so agents can cite mechanically.

**Context expansion:** given a search hit, the backend supports:

1. Neighboring chunks in the same block
   (`chunks WHERE store_id = ? AND resource_id = ? AND block_seq = ? ORDER BY seq_in_block`)
2. Nearby blocks in the same resource (`blocks WHERE resource_id = ? AND seq BETWEEN ? AND ?`)
3. Full resource block sequence (`blocks WHERE resource_id = ? ORDER BY seq`)

## 7. Metadata taxonomy

### DublinCoreMetadata (base for all resource kinds)

Dublin Core Metadata Element Set 1.1 (DCMES), all 15 elements. Repeatable elements (multi-valued)
use `Vec<String>`; singleton elements use `Option<String>`.

| Element       | Type             | Notes                                                   |
| ------------- | ---------------- | ------------------------------------------------------- |
| `title`       | `Option<String>` | Title of the resource.                                  |
| `creator`     | `Vec<String>`    | Repeatable: authors, creators.                          |
| `subject`     | `Vec<String>`    | Repeatable: topics, keywords.                           |
| `description` | `Option<String>` | Summary or abstract.                                    |
| `publisher`   | `Option<String>` | Entity responsible for making the resource available.   |
| `contributor` | `Vec<String>`    | Repeatable: additional contributors.                    |
| `date`        | `Option<String>` | Date of creation or publication (ISO 8601 recommended). |
| `r#type`      | `Option<String>` | Nature or genre of the resource.                        |
| `format`      | `Option<String>` | File format or media type.                              |
| `identifier`  | `Option<String>` | Unambiguous reference (URL, DOI, ISBN, …).              |
| `source`      | `Option<String>` | Source resource this document is derived from.          |
| `language`    | `Option<String>` | Language of the resource (BCP 47 recommended).          |
| `relation`    | `Vec<String>`    | Repeatable: related resources.                          |
| `coverage`    | `Option<String>` | Spatial or temporal extent.                             |
| `rights`      | `Option<String>` | Rights statement or license.                            |
| `date_source` | `Option<String>` | Provenance of `date` — see below. Not a DCMES element.  |

#### `date_source` (provenance of `date`)

Every site that writes `dc:date` (`DublinCoreMetadata::date`) stamps `date_source` in the same
statement, so a date is never persisted without a record of which extraction path produced it — a
date without correct provenance is considered worse than no date.
`#[serde(skip_serializing_if = "Option::is_none")]` on the field: a document with no stamped
`date_source` (every document indexed before this field existed) serializes byte-identical to
before, so introducing the field does not by itself change `metadata_hash` for the existing corpus
(see `core::ids::compute_metadata_hash`).

| Value                      | Set by                                                                        |
| -------------------------- | ----------------------------------------------------------------------------- |
| `"pdf-info"`               | PDF `/CreationDate` (Info dictionary).                                        |
| `"xmp"`                    | PDF XMP `xmp:CreateDate` (fallback when the Info dictionary has none).        |
| `"epub-opf"`               | EPUB OPF `dc:date`/published date.                                            |
| `"office-core-properties"` | docx/pptx `docProps/core.xml` `dcterms:created`.                              |
| `"html-json-ld"`           | HTML `script[type="application/ld+json"]` `datePublished`/`dateModified`.     |
| `"html-meta"`              | HTML `<meta>` date tag (`dcterms.date`, `article:published_time`, or `date`). |
| `"front-matter"`           | Markdown YAML front-matter `date:` key.                                       |
| `"feed-entry"`             | Feed connector enrichment (`published`, falling back to `updated`).           |

The feed-entry overwrite (`ingest::url_pipeline::build_resource`) sets `dc.date` and `date_source`
together in the same statement: a page parser's own `date_source` (e.g. `"html-json-ld"`, when the
fetched page itself carries JSON-LD) must never survive stamped on the feed's date — that would
misattribute the feed's publication claim to the page's own metadata.

#### Population by source format

EPUB populates the set from the OPF, whose metadata _is_ Dublin Core. PDF populates it from the Info
dictionary first, with XMP as a per-field fallback: `/Title`, `/Author` → `creator`, `/Subject` →
`description`, `/Keywords` → `subject` (split on `,` and `;`), `/CreationDate` → `date` (PDF date
syntax `D:YYYYMMDDHHmmSSOHH'mm'` parsed to ISO-8601; on parse failure the field is left empty rather
than storing the raw string), then XMP's `dc:creator`, `dc:description`, `dc:subject`,
`dc:language`, `dc:rights` and `xmp:CreateDate`.

Two fields are deliberately left empty for PDFs. `publisher` has no honest source — the Info
dictionary's nearest key, `/Producer`, is the _generating software_ ("Adobe PDF Library 15.0"), not
the publisher of the work. And `title` has no filename or first-page fallback: a PDF that carries
neither `/Title` nor XMP has no title, and inventing one would be a guess presented as data.

`format` is set by the parser from the sniffed MIME type, not read from the document.

Office (docx/pptx only — CSV has no `docProps` part, and `.odt` has no parser yet, #254) reads
`docProps/core.xml`: `dc:title` (trimmed; empty or absent falls through) and `dcterms:created`
(stored raw, untouched — `date_parsed` derivation normalizes it downstream). Title precedence is
explicit-over-heuristic: a non-empty `dc:title` wins over anytomd's H1-derived title outright, even
a stale placeholder like Word's default `"Document1"` — a quality-aware precedence that preferred a
good heading instead was considered and deliberately not built.

HTML reads a document date in precedence order (first hit wins): JSON-LD
(`script[type="application/ ld+json"]`, document order — the first entry, top-level or within an
`@graph` array, carrying `datePublished` else `dateModified`, no `@type` filtering), then
`<meta name="dcterms.date">`, then `<meta property="article:published_time">`, then the legacy
`<meta name="date">`. A script whose JSON fails to parse is skipped, not fatal — extraction falls
through to the next signal.

Markdown scans YAML front-matter for a top-level `date:` key (bare or quoted scalar; nested keys,
multi-document front-matter, and folded scalars are out of scope, #195) and stores the raw value.

A filename-based date/title fallback (e.g. `2024-01-05-post.md`) is deferred to a future PR — none
of the formats above fall back to the filename today.

### Metadata enum

```rust
enum Metadata {
    Document(DocumentMetadata),       // DC base + document-specific fields
    Conversation(ConversationMetadata), // DC base + conversation-specific fields
    Transcription(TranscriptionMetadata), // DC base + transcription-specific fields
}
```

Each variant embeds `DublinCoreMetadata` and adds kind-specific fields:

- **DocumentMetadata**: `page_count: Option<u32>`, `word_count: Option<u32>`.
- **ConversationMetadata**: `platform: Option<String>`, `message_count: Option<u32>`,
  `date_range: Option<(String, String)>`.
- **TranscriptionMetadata**: `duration_ms: Option<u64>`, `speakers: Vec<String>`,
  `media_uri: Option<String>`.

All variants expose `fn dublin_core(&self) -> &DublinCoreMetadata` for uniform access to the base
metadata fields.

**Persistence:** `Metadata` is JSON-encoded into a single `TEXT` column named `metadata_json` on
each resource record in libsql. The discriminant is the `Metadata` enum variant tag (e.g.
`{"kind":"document","dublin_core":{...},"page_count":...}`).

**Metadata unification (#130):** the flat, parser-level `DocumentMetadata` struct (a bare 15-element
Dublin Core struct that lived in the parser boundary and was easily confused with the same-named
`DocumentMetadata` variant payload above) is retired. `ParsedDocument.metadata` is
`DublinCoreMetadata` directly — the same base type every `Metadata` variant embeds — so there is
exactly one Dublin-Core-shaped struct in the codebase, not two. Resources, chunks, and citations all
carry the tagged `Metadata` enum; nothing downstream of parsing sees the untagged flat form.

**Reads (`document get`/`document list`, specs/05-surfaces.md §2, §3, §4):** CLI, HTTP, and MCP each
surface a document's registry row plus this tagged `Metadata` verbatim. Because the write path
already stored the enum in its tagged shape from the start, adding these read surfaces needed no
rewrite of stored data — they are exactly the Resource-based reads this section's shape was already
built to answer.

## 8. Extraction & parsing

### Ingestor trait (acquisition + structuring)

The `Ingestor` trait (`core/src/ingestor.rs`) is the abstraction for content acquisition and
structuring. Each ingestor knows how to connect to a source, enumerate content, and produce
`Resource`s with typed blocks.

| Method   | Signature                                                                | Notes                            |
| -------- | ------------------------------------------------------------------------ | -------------------------------- |
| `kind`   | `(&self) -> IngestorKind`                                                | Which ingestor kind this is.     |
| `ingest` | `(&self, source, config) -> impl Stream<Item = Result<Resource, Error>>` | Async stream yielding resources. |

**IngestorKind** enum: `File`, `Url`, `Notion`, `Telegram`, `Signal`, `HackMd`, `Email`,
`Transcription`, `Feed`. The enum lives in `core`; concrete ingestor implementations live outside
`core` (in `cli`, dedicated crates, or a future `ingest` crate).

**Crate boundary:** `core::Ingestor` is the contract (yields `Resource`s). Terminal interaction,
credential prompts, HTTP/API clients, and source-specific setup live outside `core`, consistent with
the "no I/O frameworks in core" invariant ([01-architecture.md](01-architecture.md) §1).

### Parser chain (file-ingestor implementation detail)

The `Parser` trait remains as the abstraction for format-specific text extraction within the **file
ingestor**. Parsers now return `Resource` (with typed blocks) instead of `ParsedDocument`. The
`markdown_to_blocks()` helper converts Markdown pulldown-cmark events to typed blocks, so existing
parsers can emit Markdown as before and convert at the boundary.

Each `Parser` is `Send + Sync` and runs synchronously (CPU-bound); callers run it under
`spawn_blocking`. Two methods:

| Method  | Signature                                                  | Notes                                                             |
| ------- | ---------------------------------------------------------- | ----------------------------------------------------------------- |
| `id`    | `(&self) -> &'static str`                                  | Stable string used in the `parsers:` config list and diagnostics. |
| `parse` | `(&self, &Probe) -> Result<Option<ParsedDocument>, Error>` | See contract below.                                               |

**Contract — three outcomes:**

- `Ok(None)` — decline; this parser does not handle the input. Control passes to the next parser in
  the chain.
- `Ok(Some(doc))` — handled successfully. First match wins; remaining parsers are not tried.
- `Err(e)` — the format was recognized but parsing failed. **Short-circuits the chain** — remaining
  parsers are NOT tried, because the failure is definitive, not a format mismatch.

`ChainParser` implements this same `Parser` trait (Composite pattern), holding an ordered
`Vec<Box<dyn Parser>>`. It is itself a `Parser` and can be nested. `build_chain(ids)` in
`extract/src/registry.rs` maps the config `parsers:` strings to concrete `Parser` instances.

### Probe

`Probe` is the fully-buffered input presented to each parser. The streaming or HTTPS read happens
once at the ingestion boundary; parsers never seek or re-fetch.

| Field / method               | Notes                                                                                   |
| ---------------------------- | --------------------------------------------------------------------------------------- |
| `bytes`                      | Full document bytes.                                                                    |
| `path_hint: Option<&str>`    | Original filename or URL path — used for file-extension hints. Advisory; may be absent. |
| `sniffed_mime: Option<&str>` | MIME type inferred before parsing. Advisory; may be wrong or `None`.                    |
| `header()`                   | Up to `PROBE_HEADER_LEN` (8 192) leading bytes for cheap magic-byte sniffing.           |

### ParsedDocument → Resource conversion

`ParsedDocument` remains as the parser output: a Markdown string + title + `DublinCoreMetadata`. The
file ingestor converts it to a `Resource` by:

1. Running `markdown_to_blocks()` on the Markdown string to produce typed blocks.
2. Wrapping `ParsedDocument.metadata` (`DublinCoreMetadata`) into
   `Metadata::Document(DocumentMetadata { dublin_core, page_count, word_count })` — see §7.
3. Computing the content hash from ordered block texts.

This conversion is a compatibility bridge. Future parsers and ingestors can emit blocks directly.

## 9. Storage schema design rationale

The unified database schema uses several design patterns to ensure referential integrity and query
performance:

- **Composite Uniqueness:** The `resources` and `chunks` tables use composite `(store_id, id)`
  uniqueness. Content-addressed IDs can collide across stores by design. Each store maintains its
  own rows. Cross-store deduplication is deferred to query-time `GROUP BY` operations.
- **Normalized Blocks:** The `blocks` table stores individual blocks as rows (not a JSON blob),
  enabling efficient context expansion queries (fetch neighboring blocks for a search hit).
- **Denormalised Store ID:** The `store_id` column is denormalised onto the `chunks` table for
  per-store filtering directly on the rowid lookup after vector or FTS5 searches.
- **Block Reference on Chunks:** Each chunk references its parent block via denormalized `block_seq`
  (no `block_id`/rowid foreign key — see §2), enabling block-level context expansion without an
  extra join, on the composite index
  `idx_chunks_store_resource_pos (store_id, resource_id, block_seq, seq_in_block)`.
- **FTS5 Content Keying:** The FTS5 virtual table `chunks_fts` uses external content keying over
  `chunks.text`. Filtering by `store_id` is performed on the `chunks` join.
- **Cascade Chain:** Foreign keys with `ON DELETE CASCADE` across the chain:
  `stores → sources → resources → blocks → chunks`. Deleting a store cleans up everything.
- **Schema Versioning:** A `schema_migrations` table is the **source of truth** for schema version;
  `PRAGMA user_version` is kept in lockstep as a cheap head marker but is never authoritative.
  Columns:

  | Column                    | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
  | ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
  | `version`                 | `INTEGER PRIMARY KEY`. Baseline is 4 (`BASELINE_VERSION`, the last pre-migration schema); the chain starts at 5.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
  | `name`                    | Short migration identifier.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
  | `applied_at`              | RFC 3339 timestamp.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
  | `down_sql`                | JSON array of statements that reverse this migration, or `NULL` if not mechanically reversible.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
  | `down_unsupported_reason` | Human-readable reason downgrade past this step is refused, or `NULL` if `down_sql` is set.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
  | `checksum`                | `blake3` over the migration's version, name, rendered up-SQL, and rendered down-SQL (or reason). Verified on every open, bounded to the already-applied prefix before `db migrate` applies anything new, and again over the full chain afterward; a mismatch is an `internal` error, not a silent continue. Verification also requires a row to _exist_ for the baseline and every applicable chain version (not just checking whatever rows happen to be present), and that each row's stored `name`/`down_sql`/`down_unsupported_reason` still match the compiled migration even when its `checksum` column reads correctly — a missing or tampered-but-checksum-intact row is treated the same as a checksum mismatch. `db downgrade` similarly requires the row history between its target and the current version to be contiguous before replaying anything. |

  `CHECK` constraint: exactly one of `down_sql` / `down_unsupported_reason` is set per row.

  **Open never migrates**, in either direction, on any surface. A version mismatch on open is a
  refusal (`invalid_config`, exit 2) with an actionable hint, not an automatic fix:
  - Legacy `0 < version < 4` (v1–v3): refused; hint points at `localdb db migrate` (which rebuilds
    destructively, behind a confirmation prompt) or deleting the database. Previously these versions
    triggered silent reinitialization on open; nothing is silent now.
  - `4 <= version < head` (pending migrations): refused; hint points at `localdb db migrate`.
  - `version > head` (store newer than this binary): refused; hint points at `localdb db downgrade`
    or upgrading localdb.

  Migrations are applied only via the explicit `localdb db migrate` /
  `localdb db downgrade [--to N]` CLI commands ([05-surfaces.md](05-surfaces.md) §2) — never by the
  HTTP daemon or MCP, which only ever surface the refusal-with-hint.

  **Downgradable by older binaries:** every migration's rendered down-SQL is stored _as data_ in
  `schema_migrations.down_sql`, so an older binary can replay it without knowing the newer schema.
  Migrations that are irreversible or expressed as Rust functions instead record
  `down_unsupported_reason`; `db downgrade` past such a step is refused cleanly, naming the
  migration and the reason, without touching the store. Freshly created stores are seeded with a
  `schema_migrations` row (including down-SQL) for every chain migration, so a brand-new store on
  the latest binary is downgradable too.

  **Three weight classes**, by authoring cost and what's allowed to run inside `db migrate`:
  1. **Fast schema DDL** — ordinary transactional runner steps.
  2. **In-DB rebuilds** (FTS5 rebuild, DiskANN index drop + recreate) — single-statement runner
     steps that may take minutes; acceptable because `db migrate` is explicit and reports per-step
     progress.
  3. **Re-embedding / re-extraction** — not runnable by the store itself, since it needs the
     embedder/extractors that live above `store-libsql`. The migration only _marks_ the work (bumps
     the required `policy_version`/`extractor_version`, truncates derived rows); the existing
     staleness machinery and incremental `localdb index` do the actual work, resumably and with
     progress. `db migrate` ends with a `localdb index` hint whenever it applied a migration of this
     class.

  **Write-twice rule:** `create_schema()` always represents _head_ DDL directly (not by replaying
  the chain) — every migration is written twice, once as a chain entry and once folded into
  `create_schema()`. A CI drift-guard test asserts baseline schema + chain output is identical to
  `create_schema()`'s output, so the two can't silently diverge.

- **Extractor Versioning:** `resources.extractor_version` tracks which parser/ingestor version
  produced the blocks, enabling selective reprocessing when extraction logic improves.

### Schema v5 (2026-07)

Schema version 5 — the first entry in the migration chain above (§9's `schema_migrations` table),
`drop_chunks_block_id_and_retag_resource_metadata` — ships this refactor's storage changes:

- `chunks.block_id` is dropped (§2, #128); the parent block is looked up by `block_seq`, not a row
  reference.
- New composite index
  `idx_chunks_store_resource_pos (store_id, resource_id, block_seq, seq_in_block)` replaces the old
  `block_id`-keyed lookup.
- `resources.metadata_json` carries the tagged `Metadata` enum encoding (§7), not the retired flat
  `DocumentMetadata`.
- `chunks.location_json` gains the optional `window_block_seqs` array (§2, #129).

**A v4 store refuses to open until migrated:** as with every schema change under the migration
framework, opening a store still at v4 fails with `invalid_config` (exit 2) pointing at
`localdb db migrate` — nothing is wiped implicitly. Running `localdb db migrate` applies this
migration (drops `chunks.block_id`, swaps the index, retags `resources.metadata_json`) in one
transaction and lands the store at v5.

**This migration is not downgradable:** `chunks.block_id` cannot be reconstructed from what remains
once dropped, so its `Down` is `Unsupported` — `localdb db downgrade` refuses cleanly past this
step, naming the reason. It also sets `needs_reindex: true`: applying it marks existing chunks stale
(see the chunk-ID paragraph below), and `db migrate` prints a `localdb index` hint after applying
it.

**Old chunk IDs are tolerated, not migrated:** chunk IDs computed under the pre-#128 formula (keyed
off `block_id`) are not translated to the new
`blake3(resource_id ‖ block_seq ‖ chunk_text ‖ seq_in_block)` formula. Instead, the chunking policy
identifier bumps (`textsplitter-md-v3` → `textsplitter-md-v4`), which changes every chunk's
`policy_version`. The existing incremental-skip check already treats a `policy_version` mismatch as
"needs reindex," so the next `localdb index` re-chunks and re-derives every chunk ID under the new
formula without any special-cased migration logic.
