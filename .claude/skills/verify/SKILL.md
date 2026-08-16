---
name: verify
description: Drive the real localdb binary end-to-end against an isolated temp config/data dir to verify a change at the CLI surface.
---

# Verifying localdb changes at the CLI surface

Build once, then drive `./target/debug/localdb` against an isolated data dir so the
machine's default config/db (often stale/old-schema) can't interfere.

```sh
TMPDIR="$HOME/../tmp" cargo build -p localdb        # see repo CLAUDE.md for TMPDIR rule
SMOKE=/path/to/tmp/smoke && mkdir -p "$SMOKE/docs"
printf '# Doc\n\nSome text.\n' > "$SMOKE/docs/a.md"
./target/debug/localdb init --config "$SMOKE/config.yaml" --json
# then edit $SMOKE/config.yaml: under `paths:` set `data: $SMOKE/data`
./target/debug/localdb --config "$SMOKE/config.yaml" store add notes
./target/debug/localdb --config "$SMOKE/config.yaml" source add "$SMOKE/docs" --store notes  # auto-indexes
./target/debug/localdb --config "$SMOKE/config.yaml" search "some text"
./target/debug/localdb --config "$SMOKE/config.yaml" status
```

## Gotchas

- **Put global flags (`--config`, `--store`, `--json`) BEFORE `search`'s query.** The query
  is `trailing_var_arg`: anything after it — including flags — is silently treated as query
  text, and the default config (possibly an old-schema db) gets used instead.
- `init` without an isolated `paths.data` opens the platform-default db and fails if that
  db's schema is behind the build. Always point `paths.data` into the smoke dir.
- Exit codes: pipelines eat them (`localdb ... | tail` makes `$?` tail's). Redirect to a
  file and check `$?` directly.
- Embedding model is cached under the platform models dir; keep `paths.models` default to
  avoid a ~700 MB re-download.
- On this macOS sandbox, every run prints `E5RT encountered an STL exception. msg = I/O
  error.` on stderr at teardown (CoreML/ANE runtime noise). Pre-existing; ignore.
- Cross-process lock probe: `( printf 'BEGIN IMMEDIATE;\n'; sleep 25 ) | sqlite3
  "$SMOKE/data/localdb.db" &` then modify a doc and `index` — the write fails with the
  RuntimeStateLocked "busy timeout" warning; non-strict exits 0 with `1 errors`, `--strict`
  exits 2. Lock released → reindex succeeds.
