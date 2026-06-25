# Proposal — Single SQLite Store

> **Transitory document.** Lives only as the review artefact for branch
> `single-sql-store`. On merge it is **deleted**; its surviving content folds into
> [specs/01](../../specs/01-architecture.md), [03](../../specs/03-config.md),
> [05](../../specs/05-surfaces.md), and `docs/architecture.md`. Do not link to this
> file from anywhere that outlives the PR.
>
> **No legacy support.** Pre-1.0, no users. Existing data files become unreadable
> after this lands; the binary refuses to start if it finds them and tells the user
> to re-add their stores. There is no migration command and no compatibility layer.

## TL;DR

One SQLite file at `<data_dir>/localdb.db` (libsql, WAL, `foreign_keys=ON`,
`busy_timeout=5000`) holds everything the binary persists: the store registry,
sources, documents, chunks, FTS5 index, and DiskANN vector index. Multi-tenancy is a
`store_id` column with foreign keys, not a separate file per store. The `RetrievalStore`
trait stays as is; the implementation becomes a thin handle `(Arc<LibsqlDb>, store_id)`
that filters by `store_id`. CLI, MCP, and the HTTP daemon share one open connection.

The win is **referential integrity end to end**: `DELETE FROM stores WHERE id = X`
cascades through `sources → documents → chunks → chunks_fts → vector index` in one
transaction. Three call-site path constructions, the in-memory `FakeStore`, the YAML
reconciliation shadow-write, and the per-file `schema_version` table all collapse.

---

## 1. File layout

```
<data_dir>/
  localdb.db                # the unified file
  localdb.db-wal            # WAL sidecar (libsql managed)
  localdb.db-shm            # shared-memory sidecar (libsql managed)
  daemon.sock               # unchanged
  models/                   # unchanged (paths.models)
```

No `stores/` directory. No `runtime-state.db`. No `.write.lock` (Decision 3 dropped it; see §3).

## 2. Schema

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA user_version = 1;   -- single source of schema version; survives VACUUM

-- Stores ---------------------------------------------------------------
CREATE TABLE stores (
    id              TEXT    PRIMARY KEY NOT NULL,    -- ULID
    name            TEXT    NOT NULL UNIQUE,
    visibility      TEXT    NOT NULL DEFAULT 'private',
    backend         TEXT    NOT NULL DEFAULT 'libsql',
    indexing_policy TEXT    NOT NULL,                -- JSON {chunking,embedding,parsers}
    policy_version  TEXT    NOT NULL,                -- effective hash
    acl             TEXT    NOT NULL DEFAULT '{}',   -- reserved
    created_at      TEXT    NOT NULL
);

-- Sources --------------------------------------------------------------
CREATE TABLE sources (
    id          TEXT PRIMARY KEY NOT NULL,           -- ULID
    store_id    TEXT NOT NULL REFERENCES stores(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,                        -- 'path' | 'url'
    root        TEXT,                                 -- absolute path  (kind=path)
    url         TEXT,                                 -- canonical URL  (kind=url)
    include     TEXT NOT NULL DEFAULT '[]',           -- JSON array of globs
    exclude     TEXT NOT NULL DEFAULT '[]',           -- JSON array of globs
    preset      TEXT NOT NULL DEFAULT 'prose',
    refresh     TEXT,                                 -- ISO-8601 duration (kind=url)
    created_at  TEXT NOT NULL,
    CHECK ((kind = 'path' AND root IS NOT NULL AND url IS NULL)
        OR (kind = 'url'  AND url  IS NOT NULL AND root IS NULL))
);
CREATE INDEX idx_sources_store_id ON sources(store_id);
CREATE UNIQUE INDEX idx_sources_store_root ON sources(store_id, root) WHERE root IS NOT NULL;
CREATE UNIQUE INDEX idx_sources_store_url  ON sources(store_id, url)  WHERE url  IS NOT NULL;

-- Documents ------------------------------------------------------------
CREATE TABLE documents (
    rowid           INTEGER PRIMARY KEY,              -- FTS5 / vec rowid join
    store_id        TEXT    NOT NULL REFERENCES stores(id)  ON DELETE CASCADE,
    id              TEXT    NOT NULL,                 -- blake3(uri ‖ content_hash)
    source_id       TEXT    NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    source_kind     TEXT    NOT NULL,
    uri             TEXT    NOT NULL,
    title           TEXT,
    mime            TEXT,
    content_hash    TEXT    NOT NULL,
    fetched_at      TEXT    NOT NULL,
    origin_store    TEXT    NOT NULL,                 -- ≠ store_id after federation
    policy_version  TEXT    NOT NULL,
    metadata        TEXT    NOT NULL DEFAULT '{}',
    share_path      TEXT,                             -- reserved, empty in MVP
    UNIQUE (store_id, id)
);
CREATE INDEX idx_documents_store_uri ON documents(store_id, uri);
CREATE INDEX idx_documents_source_id ON documents(source_id);

-- Chunks ---------------------------------------------------------------
CREATE TABLE chunks (
    rowid          INTEGER PRIMARY KEY,
    store_id       TEXT    NOT NULL,                  -- denormalised for filter
    id             TEXT    NOT NULL,                  -- blake3(document_id ‖ text ‖ span)
    document_id    TEXT    NOT NULL,
    seq            INTEGER NOT NULL,
    text           TEXT    NOT NULL,
    span_start     INTEGER NOT NULL,
    span_end       INTEGER NOT NULL,
    heading_path   TEXT    NOT NULL,                  -- JSON
    embedding      {col_type} NOT NULL,               -- F32_BLOB(dim) | F1BIT_BLOB(dim)
    UNIQUE (store_id, id),
    FOREIGN KEY (store_id, document_id)
        REFERENCES documents(store_id, id) ON DELETE CASCADE
);
CREATE INDEX idx_chunks_store_doc ON chunks(store_id, document_id);
CREATE INDEX chunks_vec_idx
    ON chunks(libsql_vector_idx(embedding,
        'metric=cosine',
        'max_neighbors=64',
        'compress_neighbors=float8'));     -- tuning from PR #92 review feedback

-- FTS5 (external content over chunks.text) + sync triggers -----------------
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    text, content='chunks', content_rowid='rowid'
);
CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.rowid, old.text);
END;
CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', old.rowid, old.text);
    INSERT INTO chunks_fts(rowid, text) VALUES (new.rowid, new.text);
END;
```

### Design notes

- **Composite `(store_id, id)` uniqueness** on `documents` and `chunks`. Content-addressed
  IDs (spec [02](../../specs/02-domain-model.md) §3) collide across stores by design
  — two stores indexing the same file derive the same chunk id. Composite uniqueness
  keeps "each store has its own row" semantics and leaves cross-store dedup
  (open item [B2](../design-decisions.md#b2--cross-store-deduplication-semantics))
  as a future `GROUP BY id` query, not a today decision.
- **Composite FK `chunks(store_id, document_id) → documents(store_id, id)`** so the
  cascade chain `stores → documents → chunks` fires in one transaction. `chunks_ad`
  cascades through to `chunks_fts`; libsql vector-index auto-maintenance handles the
  vector rows. Confirmed pattern — librarian cited
  [nearai/ironclaw](https://github.com/nearai/ironclaw) and
  [Sibyl-Memory](https://github.com/Sibyl-Labs/Sibyl-Memory) as precedents combining
  FTS5 + `libsql_vector_idx` in one file.
- **`store_id` on `chunks`** is duplicated from the join through `documents` so the
  per-store filter applies directly on the rowid lookup after `vector_top_k(...)` and
  `chunks_fts MATCH ...` — no extra join through `documents` just to gate by tenant.
- **FTS5 stays content-keyed.** Adding `store_id UNINDEXED` would require pushing the
  column through all three triggers; filtering on the `chunks` join is equally
  efficient because FTS5 returns rowids and `chunks` is rowid-keyed.
- **`schema_version` table dropped.** `PRAGMA user_version` is the idiomatic SQLite
  mechanism and survives `VACUUM`.

### Query shape (dense, with store filter)

```sql
SELECT c.id, c.document_id, c.seq, ..., d.metadata,
       vector_distance_cos(c.embedding, {qvec_sql}) AS distance
FROM   vector_top_k('chunks_vec_idx', {qvec_sql}, {fetch_k}) AS v
JOIN   chunks    c ON c.rowid = v.id
JOIN   documents d ON d.store_id = c.store_id AND d.id = c.document_id
WHERE  c.store_id IN ({store_ids})
{extra_filters}
ORDER BY distance ASC
LIMIT  {limit};
```

The `fetch_k = limit * 3` widening (regression test
`dense_search_with_filter_returns_matching_chunks`) continues to cover this new filter.
BM25 mirrors the same shape (`chunks_fts MATCH ?` + filter on the chunks join).

## 3. Lifecycle

```
                ┌───────────────────────────────┐
                │  LibsqlDb  (Arc<...>)          │
                │  - single connection (mutex)   │   one per process
                │  - opens localdb.db (WAL)      │
                └──────┬───────────────┬─────────┘
                       │               │
        ┌──────────────┴────┐    ┌─────┴─────────────────┐
        │ RuntimeStateApi   │    │ StoreHandle           │
        │  - stores CRUD    │    │  - takes store_id     │
        │  - sources CRUD   │    │  - implements         │
        │  - effective cfg  │    │    RetrievalStore     │
        └───────────────────┘    └───────────────────────┘
```

Each surface (CLI, MCP, daemon) opens **one** `LibsqlDb`, then constructs one
`RuntimeStateApi` and as many `StoreHandle`s as it queries. `RetrievalStore` trait
signatures are unchanged.

The advisory `.write.lock` file is **gone** (Decision 3). SQLite is the single
concurrency primitive: WAL lets unlimited readers run concurrently with one writer;
concurrent writers serialise via `busy_timeout=5000`; an exhausted busy-timeout maps
to the existing `runtime_state_locked` error (exit 4, spec
[05](../../specs/05-surfaces.md) §5).

**The driver is multi-process as the first-class topology, not "CLI coexists with
daemon" as a side-effect.** Multiple stdio MCP servers (one per agent client — Claude,
Cursor, IDE plugins), a CLI session running `localdb index`, and an optional `localdb
serve` daemon may all share one data dir. The daemon is just another process; it no
longer "owns" the data dir. Daemon discovery stays socket-based (`daemon.sock`);
"only one daemon per data dir" remains true via socket bind, not file lock. The
`store_locked` error code collapses into `runtime_state_locked` since they now
describe the same condition. Layering a process-level fd-lock on top of SQLite's
engine-level lock would be double-locking with no case SQLite alone doesn't already
handle — KISS wins.

## 4. Implementation order

Three reviewable PRs stacked on `single-sql-store`:

1. **Schema + `LibsqlDb` + `RuntimeStateApi` + `StoreHandle`.** Add the new types
   alongside the existing ones; port the conformance suite to run against both shapes
   so the cutover in PR 2 is a search-and-replace, not a rewrite.
2. **Surface migration.** `cli/`, `mcp/`, `server/` switch to `LibsqlDb`. Inline
   `data_dir.join("stores").join(name).join("store.db")` paths and the YAML
   reconciliation shadow-write go away. HTTP daemon's `FakeStore` and in-memory source
   map are deleted (Decision 2 sets whether YAML-store indexability also lands here).
3. **Cleanup + lock removal + spec sync.** Delete the old `RuntimeStateDb`, old
   `LibsqlStore::open(path)`, old per-store `schema.rs`. Add the "refuse to start with
   legacy layout" guard (detect `runtime-state.db` or `stores/` → log re-add
   instructions → exit 2). Drop the `.write.lock` file (Decision 3): remove the lock
   acquisition in CLI/daemon, collapse `store_locked` into `runtime_state_locked`,
   simplify `localdb serve` startup. Update specs [01](../../specs/01-architecture.md)
   §3 §6, [03](../../specs/03-config.md) §4, [05](../../specs/05-surfaces.md) §3 §5,
   [docs/architecture.md](../architecture.md). **Delete this proposal file.**

## 5. What this enables

- Cascade deletes for free — `store remove`, `source remove`, and `delete_by_document`
  become single SQL statements.
- One-query cross-store search — `WHERE c.store_id IN (...)` replaces N file opens.
- The HTTP daemon stops lying ([known gap §2](../architecture.md#known-gaps) closes).
- Atomic source-add-and-index — one transaction instead of three call sites and two
  files.
- `share_path` provenance has a real place to live against the unified store registry.
- One file to back up (`VACUUM INTO`, `sqlite3_backup_*`).

## 6. Risks

| Risk | Notes |
|---|---|
| One file, one writer lane in WAL | SQLite WAL allows many concurrent readers; concurrent writers serialise via `busy_timeout=5000`. Exhausted timeout surfaces as `runtime_state_locked` (exit 4) — existing error code. Spec [01](../../specs/01-architecture.md) §3 single-writer rule moves from "one process holds the fd-lock" to "SQLite admits one writer at a time". See §3 for the multi-process concurrency model. |
| One file, one corruption blast radius | Acceptable; backup becomes trivial in exchange. |
| Composite-FK cascade × libsql vector-index auto-maintenance | Add a regression test in PR 1: `DELETE FROM stores ...` → assert `SELECT COUNT(*) FROM chunks` = 0 and vector index is empty. |
| `FakeStore` removal touches server integration tests | Mechanical — they switch to a real tmpdir `LibsqlDb`. |
| Large `DELETE FROM stores` cascade could be slow | Tunable later with batched pre-delete on `chunks`; not a v1 concern. |

## 7. Decisions

| # | Choice | Notes |
|---|---|---|
| 1 | File name: **`localdb.db`** | Product-named, visible in `ls`. |
| 2 | Scope: **merge + daemon FakeStore fix** | Closes [known gap §2](../architecture.md#known-gaps) — HTTP daemon stops using in-memory store, opens the unified DB like CLI. ~30 LoC of wiring in PR 2. YAML-store indexability ([gap §4](../architecture.md#known-gaps)) **not** in scope unless it falls out trivially. Source path validation, `search --store <unknown>` exit code, bundle ID naming explicitly deferred. |
| 3 | Write lock: **drop `.write.lock`** | SQLite WAL + `busy_timeout=5000` + existing `runtime_state_locked` error is the single concurrency primitive. Multiple MCPs, CLI sessions, and an optional daemon share one data dir as peers — the daemon is not special. `store_locked` collapses into `runtime_state_locked`. Lands in PR 3 alongside the spec sync. |
| 4 | Normalization: **full** | Real columns + CHECK constraints + partial UNIQUE indexes on `(store_id, root)` / `(store_id, url)`, exactly as shown in §2. No JSON blobs on `stores`/`sources`. |
