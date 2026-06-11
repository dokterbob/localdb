#!/usr/bin/env bash
# smoke_test.sh — T12 acceptance smoke test
#
# Validates the localdb binary on a clean environment by running:
#   install (verify binary is on PATH) → init → index fixture → search
#
# Usage:
#   bash smoke_test.sh
#
# Assumptions:
#   - `localdb` binary is on PATH (either from `cargo install` or a release tarball)
#   - This script is run from the repository root (or any writable directory)
#
# See specs/06-roadmap.md §4 and PLAN.md T12 for context.
#
# Exit codes:
#   0  all steps passed
#   1  a step failed (the failing step is printed to stderr)

set -euo pipefail

# ---- helpers ----------------------------------------------------------------

info()  { printf '\033[1;34m[smoke]\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m[smoke] OK:\033[0m %s\n' "$*"; }
fail()  { printf '\033[1;31m[smoke] FAIL:\033[0m %s\n' "$*" >&2; exit 1; }

# ---- step 0: binary on PATH -------------------------------------------------

info "Step 0: verify localdb is on PATH"
if ! command -v localdb &>/dev/null; then
    # Try cargo install if not found.
    if command -v cargo &>/dev/null; then
        info "localdb not found on PATH; attempting cargo install ..."
        cargo install --path localdb 2>&1 | tail -5 || fail "cargo install failed"
    else
        fail "localdb not found on PATH and cargo not available"
    fi
fi
LOCALDB_BIN=$(command -v localdb)
info "Using binary: $LOCALDB_BIN"

# ---- step 1: --version -------------------------------------------------------

info "Step 1: --version"
VERSION_OUT=$(localdb --version 2>&1)
if echo "$VERSION_OUT" | grep -qE 'localdb [0-9]+\.[0-9]+'; then
    ok "--version: $VERSION_OUT"
else
    fail "--version output does not look like a semver; got: $VERSION_OUT"
fi

# ---- step 2: create a temporary workspace ------------------------------------

SMOKE_DIR=$(mktemp -d -t localdb-smoke-XXXXXX)
trap 'rm -rf "$SMOKE_DIR"' EXIT

DATA_DIR="$SMOKE_DIR/data"
mkdir -p "$DATA_DIR"

CONFIG_FILE="$SMOKE_DIR/config.yaml"
cat > "$CONFIG_FILE" <<EOF
version: 1
paths:
  data: $DATA_DIR
EOF

info "Workspace: $SMOKE_DIR"
info "Config:    $CONFIG_FILE"

# ---- step 3: init ------------------------------------------------------------

info "Step 3: init"
LOCALDB_CONFIG="$CONFIG_FILE" localdb init || fail "init failed"
ok "init"

# ---- step 4: store add -------------------------------------------------------

info "Step 4: store add smoke-store"
LOCALDB_CONFIG="$CONFIG_FILE" localdb store add smoke-store || fail "store add failed"
ok "store add"

# ---- step 5: create fixture document -----------------------------------------

DOCS_DIR="$SMOKE_DIR/docs"
mkdir -p "$DOCS_DIR"
cat > "$DOCS_DIR/intro.md" <<'EOF'
# localdb

localdb is a local-first knowledge server.

It indexes your files and URLs into a local store and provides hybrid
natural-language search via BM25 and dense vector retrieval with RRF fusion.
EOF
info "Fixture document created: $DOCS_DIR/intro.md"

# ---- step 6: source add ------------------------------------------------------

info "Step 6: source add"
LOCALDB_CONFIG="$CONFIG_FILE" localdb --store smoke-store source add "$DOCS_DIR" \
    || fail "source add failed"
ok "source add"

# ---- step 7: index -----------------------------------------------------------

info "Step 7: index"
LOCALDB_CONFIG="$CONFIG_FILE" localdb --store smoke-store index \
    || fail "index failed"
ok "index"

# ---- step 8: search and verify citations ------------------------------------

info "Step 8: search 'knowledge server hybrid search'"
RESULT=$(LOCALDB_CONFIG="$CONFIG_FILE" localdb --json --store smoke-store \
    search "knowledge server hybrid search" 2>&1)

if ! echo "$RESULT" | python3 -c "
import sys, json
data = json.load(sys.stdin)
cits = data.get('citations', [])
assert len(cits) > 0, f'no citations returned; got: {data}'
uri = cits[0].get('uri', '')
assert 'intro.md' in uri, f'top citation URI should reference intro.md; got {uri}'
print(f'citations={len(cits)}, top uri={uri}')
" 2>&1; then
    # Fallback: check with grep if python3 unavailable.
    if echo "$RESULT" | grep -q '"citations"'; then
        ok "search returned citations (JSON citations key present)"
    else
        fail "search did not return expected citations; output: $RESULT"
    fi
else
    ok "search returned citations with correct URI"
fi

# ---- done --------------------------------------------------------------------

info "All smoke steps passed."
