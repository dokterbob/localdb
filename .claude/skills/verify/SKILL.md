---
name: verify
description: Drive the real localdb binary end-to-end against an isolated temp config/data dir to verify a change at the CLI surface.
---

# Verifying localdb changes at the CLI surface

Build once, then drive `./target/debug/localdb` against an isolated data dir so the
machine's default config/db (often stale/old-schema) can't interfere. No explicit init is
needed: a minimal config with `paths.data` is enough, and the database scaffolds on first
use.

```sh
TMPDIR="$HOME/../tmp" cargo build -p localdb        # see repo CLAUDE.md for TMPDIR rule
SMOKE=/path/to/tmp/smoke && mkdir -p "$SMOKE/docs"
printf '# Doc\n\nSome text.\n' > "$SMOKE/docs/a.md"
printf 'version: 1\npaths:\n  data: %s/data\n' "$SMOKE" > "$SMOKE/config.yaml"
./target/debug/localdb --config "$SMOKE/config.yaml" store add notes
./target/debug/localdb --config "$SMOKE/config.yaml" source add "$SMOKE/docs" --store notes  # auto-indexes
./target/debug/localdb --config "$SMOKE/config.yaml" search "some text"
./target/debug/localdb --config "$SMOKE/config.yaml" status
```

## Gotchas

- Always isolate `paths.data` into the smoke dir; the platform-default db may be on an old
  schema and unrelated to your change.
- Put global flags (`--config`, `--store`, `--json`) before the `search` query.
- Exit codes: pipelines eat them (`localdb ... | tail` makes `$?` tail's). Redirect to a
  file and check `$?` directly.
- The embedding model is cached under the platform models dir; keep `paths.models` default
  to avoid a ~700 MB re-download.
- Cross-process lock probe: `( printf 'BEGIN IMMEDIATE;\n'; sleep 25 ) | sqlite3
  "$SMOKE/data/localdb.db" &` then modify a doc and `index` — the write surfaces the
  RuntimeStateLocked "busy timeout" warning; non-strict exits 0 with `1 errors`, `--strict`
  exits nonzero. Lock released → reindex succeeds.
