# localdb — Deferred Design Decisions

This document records design items that were explicitly scoped out of the v0.1.0 issue sweep
because they require cross-cutting decisions before code can be written. Each entry states the
problem, the affected code, the available options, and a recommendation — but no code has been
changed. A follow-up ticket or PR should resolve each item before implementing.

See also [docs/architecture.md#known-gaps](architecture.md#known-gaps) for runtime-visible gaps
and the [specs/](../specs/) tree for authoritative design documents.

---

## A7 — `policy_version` does not hash chunking parameters per source

**Problem.** `core/src/config/policy.rs:28` computes `policy_version` from the global
`IndexingPolicyConfig` (embedding provider + model + chunking preset overrides). However,
`cli/src/lib.rs` constructs the policy from a hardcoded `ChunkerConfig::prose()` default
rather than the resolved per-source preset. If two sources use different chunking presets
(e.g. `prose` vs `code`), or if the global preset overrides change, the policy hash may not
reflect the actual parameters used to chunk individual chunks, causing incremental indexing
to skip re-chunking when it should not.

**Options.**
1. Hash the *resolved effective parameters* (`target_chars`, `overlap_chars`, preset name)
   rather than the raw config struct. The hash is already content-addressed so no schema
   change is needed.
2. Store the policy hash per-source (rather than globally) and recompute it when a source's
   preset changes.
3. Accept the current behaviour: global hash is good enough for v1 since per-source overrides
   are rare and users can force a full re-index with `localdb index --force`.

**Recommendation.** Option 1 — resolve the effective chunker params before computing the hash;
change `canonical_policy_json` to accept the resolved `ChunkerConfig` alongside the embedding
config. Low complexity, fixes the invariant correctly.

---

## A8 / B4 — Search pagination offset is computed but never applied; `total_candidates` is pre-dedup

**Problem.** `server/src/handlers.rs:342` reads a cursor/offset parameter from the request and
parses it into `offset`, but `core` never receives the offset — the full ranked list is
returned and `handlers.rs:380` slices from index 0 instead of `offset`. Separately,
`total_candidates` in `SearchResponse` is the raw RRF-fused count before deduplication
(`search.rs:260`), so `next_cursor` arithmetic is incorrect when duplicates are removed.

Fixing this correctly for a hybrid BM25+dense pipeline requires a decision on the paging model:

**Options.**
1. **Over-fetch + slice.** Fetch `offset + limit + dedup_budget` results from the store, fuse,
   dedup, then slice. Simple; works for moderate offsets. Degrades as `offset` grows (must
   re-fetch all preceding pages from each leg).
2. **Stateful cursor.** Store fused results server-side keyed by a cursor token, page through
   them. Requires session storage and invalidation logic.
3. **No pagination (MVP).** Remove the `next_cursor` field from the response schema; return all
   results up to a server-enforced max (`limit ≤ 100`). Simplest; acceptable for v1 where
   result sets are small.

**Recommendation.** Option 3 for v1: remove the `next_cursor` field and cap `limit` at 100.
Document the limitation in `specs/05-surfaces.md §3`. Revisit with Option 1 if result sets
grow large.

---

## B2 — Cross-store deduplication semantics

**Problem.** `core/src/search.rs:240` fuses results from multiple stores but does not deduplicate
across stores. Two stores that have indexed the same document (same `content_hash`) will return
duplicate citations in a multi-store search.

**Decision needed:** Are identical content-hash chunks across stores duplicates (collapse to one
citation, picking the store with the highest score) or are they distinct (both citations are valid
because different stores may have different metadata, access controls, or provenance)?

**Options.**
1. **Collapse by content hash.** After RRF fusion, deduplicate by `chunk.content_hash`; keep the
   entry with the highest fused score. The losing citation's store is lost.
2. **Merge provenance.** Keep one citation but attach a list of stores that match (multi-provenance
   citation shape — requires a spec change to `Citation`).
3. **No dedup (current).** Return all citations; callers deduplicate. Simplest; caller gets full
   information.

**Recommendation.** Option 3 for v1. Document in `specs/04-search-pipeline.md §5`. Revisit if
multi-store deployments consistently report duplicate citations as a UX problem.

---

## B3 — Rerank seam re-attaches store metadata by index position

**Problem.** `core/src/search.rs:278` implements the rerank seam as a no-op, then re-attaches
`(store_id, store_name)` by index position (line 283). This is correct while reranking is a
no-op (order is preserved), but will silently produce wrong store attributions if a real reranker
reorders results.

**Decision needed before implementing reranking:** Define the interface. Two shapes:

**Options.**
1. **Carry store metadata through the reranker.** Pass `Vec<(FusedChunkEntry, StoreId, StoreName)>`
   to the reranker instead of stripping metadata. Rerankers must accept and pass through opaque
   metadata. Safe but pollutes the reranker interface.
2. **Re-join by chunk ID after reranking.** Strip metadata before the reranker, then re-join by
   `chunk.id` after. Requires a `HashMap<ChunkId, (StoreId, StoreName)>` built before the call.
   Cleaner interface; no performance cost for small result sets.
3. **Move reranking inside each store's result set** before fusion. Simplest if reranking is
   always store-local (e.g. cross-encoder over a single store's BM25 hits). Not possible for
   cross-store rerankers.

**Recommendation.** Option 2 — re-join by chunk ID. Implement it at the same time as the first
real reranker so the interface is tested against a non-trivial case.

---

## E1 — Structured MCP tool results (implementation deferred, spec already decided)

**Problem.** `mcp/src/tools.rs:240` returns tool results as JSON-serialized strings inside a
`text` content block. The MCP spec (and `specs/05-surfaces.md §4`) mandate structured content
blocks (`application/json` MIME type in the `content` array). This makes the MCP surface
usable but not spec-compliant; callers that parse the `content` array's MIME type will see
plain text.

**Status.** This is **spec-decided, not a design question** — the spec is clear. It is deferred
because changing the content shape is a breaking change for any existing MCP client that has
adapted to the current text-only output. The implementation work is well-scoped.

**Recommended path.** Introduce a feature flag (`LOCALDB_MCP_STRUCTURED_OUTPUT=1`) for a
transition window, then remove the flag once downstream clients (Claude Desktop, IDE plugins)
have updated. Target: v0.2.0.

---

## A9-charset — Allowed character set for store names

**Note.** The traversal-safety subset of A9 was fixed in the issue sweep (Wave 4): store names
that are empty, equal to `/`, or contain `..` components are rejected with exit 2. The remaining
open question is the *positive* charset — which characters are explicitly allowed.

**Problem.** Store names are used as directory names under `{data_dir}/stores/{name}/`. Different
filesystems allow different characters. An overly permissive allowlist causes opaque OS errors at
index time; an overly restrictive one frustrates users with multi-word or non-ASCII store names.

**Options.**
1. **ASCII-safe alphanumeric + separators** — `[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}`. Conservative;
   no filesystem surprises; matches common CLI conventions.
2. **Unicode word characters** — `\p{L}\p{N}[\p{L}\p{N}_-]*`. Allows non-ASCII names (e.g.
   Japanese, Arabic). Requires Unicode-aware filename normalization (NFC/NFD) to avoid
   duplicate stores on case-folding filesystems.
3. **Filesystem-native validation** — attempt a `mkdir` under a temp directory and return the OS
   error. Maximally permissive; non-deterministic across platforms.

**Recommendation.** Option 1 for v1. Document the regex in `specs/03-config.md §2` and the error
message in `specs/05-surfaces.md §5`. Relax to Option 2 in a later release if international
users request it.

---

## A6-atomicity — True crash-atomic upsert in LanceDB

**Note.** The immediate risk (embed failure wiping an existing doc) was fixed in Wave 3 by
reordering to embed-before-delete. The deeper atomicity question remains.

**Problem.** `store-lancedb/src/lib.rs` implements `upsert_chunks` as delete-old-rows then
append-new-rows. If the process crashes between delete and append, the document is partially
removed from the store. LanceDB's underlying Lance format supports transactions, but the Rust
client does not yet expose a `BEGIN/COMMIT` API.

**Options.**
1. **Shadow write + atomic rename.** Write new chunks to a staging fragment, delete old chunks,
   then commit. Requires LanceDB's `merge_insert` / fragment API to be available for the chunk
   table.
2. **Tombstone pattern.** Keep old chunks but mark them with a `deleted` flag; compact on a
   background sweep. Increases query complexity (filter tombstones on every read).
3. **Accept current semantics + document.** Crash during indexing leaves the store in a
   recoverable state: re-running `localdb index` re-ingests the affected document. No partial
   data is served because the delete runs before the append. Low real-world risk.

**Recommendation.** Option 3 for v1, with a `localdb repair` command (see `specs/06-roadmap.md`)
for the long tail. Revisit Option 1 when LanceDB's Rust client exposes transactional fragment
operations.
