-- Manual v4 -> v5 migration for localdb's unified libsql database.
--
-- Schema v5 (specs/02-domain-model.md §2/§9, specs/03-config.md, #128, #129):
--   1. `chunks.block_id` is dropped. The canonical block reference is now the
--      triple (store_id, resource_id, block_seq) -- blocks are looked up by
--      sequence number, not by a synthetic blocks.rowid foreign key. rowids
--      are not stable across a replace (delete+insert of a resource mints new
--      block rows), and window chunks (#129) need to reference a *set* of
--      block sequence numbers, which a single scalar FK cannot express.
--   2. The old `idx_chunks_store_resource(store_id, resource_id)` index is
--      replaced by the composite `idx_chunks_store_resource_pos(store_id,
--      resource_id, block_seq, seq_in_block)`.
--   3. `resources.metadata_json` moves from the retired flat, untagged
--      Dublin-Core-only shape to the tagged `Metadata` enum encoding: the same
--      flat object plus a `"kind"` discriminator (`"document"` for every
--      pre-v5 row -- conversation/transcription resource kinds did not exist
--      before the block model) and the two `DocumentMetadata`-specific fields
--      (`page_count`, `word_count`), explicitly nulled since they were never
--      populated by the flat encoding. See core/src/metadata.rs: `Metadata`
--      is `#[serde(tag = "kind")]` over structs that `#[serde(flatten)]` a
--      `DublinCoreMetadata`, so the tagged JSON is exactly the old flat object
--      with `"kind"`, `"page_count"`, and `"word_count"` added.
--
--      Note: `blocks.metadata_json` (BlockKind, tagged by a `"type"` field)
--      and the `chunks` table (which has no `metadata_json` column at all,
--      only `location_json`) do not need this treatment -- the block model
--      that introduced `blocks.metadata_json` always wrote the tagged shape,
--      there is no legacy flat form to repair there.
--
-- What is intentionally NOT migrated:
--   - Chunk ids computed under the pre-#128 formula (keyed off the dropped
--     `block_id`) are left as-is; they are not translated to the new
--     `blake3(resource_id || block_seq || chunk_text || seq_in_block)`
--     formula. Instead, the chunking policy identifier bumps
--     (`textsplitter-md-v3` -> `textsplitter-md-v4`), which changes every
--     chunk's `policy_version`. The existing incremental-skip check already
--     treats a `policy_version` mismatch as "needs reindex", so the next
--     `localdb index` re-chunks and re-derives every chunk id under the new
--     formula without any special-cased migration logic here.
--   - `location_json` rows written before #129 (i.e. without a
--     `window_block_seqs` key) are left as `{"start": N, "end": N}`; the
--     reader (`store-libsql/src/tenant/rows.rs`) treats a missing
--     `window_block_seqs` key as an empty vector, which is the correct value
--     for every chunk that was never part of a message-sliding-window preset.
--
-- Usage: run this script against your existing `localdb.db` (v4) BEFORE
-- starting the v5 build of `localdb`. If you skip this script, `localdb` will
-- detect the version mismatch on open, print a warning, and silently wipe and
-- reinitialise every table (stores, sources, resources, blocks, chunks,
-- credentials) -- the default pre-release "reinitialize, don't migrate"
-- policy. This script exists for anyone who wants to keep their stores,
-- sources, and resource metadata across that bump instead of re-ingesting
-- from scratch.
--
-- Example: sqlite3 /path/to/localdb.db < docs/migrations/v4-to-v5.sql

-- 1. Drop the retired block_id column from chunks.
ALTER TABLE chunks DROP COLUMN block_id;

-- 2. Replace the old two-column chunk index with the new composite index.
DROP INDEX IF EXISTS idx_chunks_store_resource;
CREATE INDEX IF NOT EXISTS idx_chunks_store_resource_pos
    ON chunks(store_id, resource_id, block_seq, seq_in_block);

-- 3. Rewrite resources.metadata_json from the old flat Dublin Core shape to
--    the new tagged Metadata::Document shape. Only touches rows that are
--    valid JSON and don't already carry a "kind" tag (idempotent: re-running
--    this script on an already-migrated database is a no-op for this step).
UPDATE resources
SET metadata_json = json_set(
    metadata_json,
    '$.kind', 'document',
    '$.page_count', NULL,
    '$.word_count', NULL
)
WHERE json_valid(metadata_json)
  AND json_extract(metadata_json, '$.kind') IS NULL;

-- 4. Stamp the database as v5 so `localdb` accepts it without triggering the
--    wipe+reinit path on next open.
PRAGMA user_version = 5;
