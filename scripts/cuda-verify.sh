#!/usr/bin/env bash
#
# cuda-verify.sh — real-GPU verification of localdb's CUDA execution-provider support
# (issue #76). Runs on a friend's NVIDIA machine, ONCE, against a published release
# tarball. No repo checkout is required.
#
# ---------------------------------------------------------------------------------
# USAGE
#
#   ./cuda-verify.sh <release-tarball-url> [--dry-run]
#
#   <release-tarball-url>   Direct URL to a linux x86_64 release tarball, e.g.
#                           https://github.com/<org>/<repo>/releases/download/vX.Y.Z/localdb-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
#                           (see .github/workflows/release.yml for the naming scheme).
#
#   --dry-run               Optional. Skips every network/localdb step; only runs the
#                            environment-evidence step (nvidia-smi / ldconfig / uname /
#                            glibc) and scaffolds the report. Lets this script be tested
#                            on a machine with no NVIDIA GPU (e.g. a Mac). Missing GPU
#                            tooling is reported as SKIP/INFO, not FAIL, and the script
#                            exits 0. The tarball URL is not required in --dry-run mode,
#                            but if given it is still validated as non-empty and ignored.
#
# WHAT IT DOES (full run, non-dry-run)
#
#   1. Environment evidence   — nvidia-smi header, ldconfig -p for libcudnn.so.9 /
#                                libcudart.so.12, uname -a, glibc version.
#   2. Setup                  — downloads + extracts the tarball into a scratch workdir;
#                                isolates ALL localdb state (XDG_CACHE_HOME,
#                                XDG_DATA_HOME, XDG_CONFIG_HOME, XDG_STATE_HOME, and HOME
#                                itself) under that workdir so the run never touches the
#                                operator's real caches/config, and so the ~706 MB
#                                embedding-model download and the ~196 MB CUDA-flavored
#                                ONNX Runtime download are both attributable and fully
#                                cleanable. Creates 10 short, distinctive fixture
#                                markdown documents for deterministic search.
#   3. Forced CUDA run        — `embedding.provider: local-cuda`, RUST_LOG=debug,
#                                `localdb index` then 3 `localdb search` queries. Asserts
#                                exit 0 on every command, that the ORT CUDA download
#                                fired (log line + cache dir), and that no
#                                "CUDA execution provider unavailable" error appeared.
#   4. CPU reference run      — fresh store, same fixtures, `provider: local-onnx`, same
#                                3 queries.
#   5. Parity check           — top-5 chunk_id overlap (>= 4/5, top-1 identical) between
#                                the CUDA and CPU runs, per query. Validates that the two
#                                execution providers are index-interchangeable on real
#                                hardware. Uses `jq` if available, else a grep/awk
#                                fallback (both parse the CLI's `--json` output).
#   6. Auto-detection run     — fresh store, `provider: local` (automatic mode). Asserts
#                                the factory's "NVIDIA/CUDA stack detected" info line
#                                fired, i.e. automatic mode chose CUDA on this machine.
#   7. Report                 — every step's PASS/FAIL/SKIP plus evidence is appended to
#                                ./cuda-verify-report.txt (created in the CURRENT
#                                working directory, not the scratch workdir) as the
#                                script runs, and a final summary block (hardware,
#                                driver, date, overall PASS/FAIL) is printed and
#                                appended. Paste that file back for review. Exits
#                                non-zero if any step failed.
#
# EXACT SOURCE STRINGS THIS SCRIPT GREPS FOR (verified against this checkout; re-verify
# if the embed crate's tracing call sites change):
#
#   - embed/src/ort_download.rs:180-184  tracing::info!(url, approx_mb,
#       "downloading ONNX Runtime (one-time, cached)…")
#         -> grepped as the literal (ASCII) substring "downloading ONNX Runtime"
#   - embed/src/ort_download.rs:204      tracing::info!(url, "ONNX Runtime download complete")
#   - embed/src/ort_download.rs:134      CUDA_LINUX_X64.cache_subdir = "1.24.4-cuda"
#   - embed/src/ort_runtime.rs:362-365   cache_dir_for(rt) = dirs::cache_dir()
#       .join("localdb").join("ort").join(rt.cache_subdir)
#         -> on Linux, dirs::cache_dir() honors XDG_CACHE_HOME first, so the CUDA
#            payloads land at "$XDG_CACHE_HOME/localdb/ort/1.24.4-cuda/"
#   - embed/src/ort_runtime.rs:311-326   tracing::info!(path, cache_subdir,
#       "initializing ONNX Runtime (downloaded)")
#         -> with cache_subdir="1.24.4-cuda" this is direct evidence the CUDA-flavored
#            ONNX Runtime (not the CPU flavor) was committed for the process
#   - embed/src/factory.rs:564-567       tracing::info!("NVIDIA/CUDA stack detected; \
#       using CUDA-enabled ONNX Runtime (CUDA preferred, automatic CPU fallback)")
#         -> fired by automatic ("local") mode when it chooses CUDA
#   - embed/src/factory.rs:546-549       tracing::info!(?status, "no complete \
#       NVIDIA/CUDA stack detected; using CPU ONNX Runtime")
#         -> fired by automatic mode when it does NOT attempt CUDA (negative case;
#            grepped-for absence would indicate the friend's machine wasn't detected)
#   - embed/src/cuda_ep.rs:199-204       cuda_unavailable_error() always begins
#       "CUDA execution provider unavailable: "
#         -> its ABSENCE from stderr is asserted after every local-cuda command, as a
#            defensive check independent of exit codes
#   - embed/src/model_cache.rs:85-90     ModelCache::default_cache_dir() = dirs::cache_dir()
#       .join("localdb").join("models")
#
# CLI invocation pattern verified against smoke_test.sh (repo root) and
# cli/src/cmds/{init,search}.rs:
#   - LOCALDB_CONFIG=<path> is honored in place of --config (localdb/src/main.rs).
#   - `localdb init` is non-interactive as long as the config file already exists
#     (cli/src/cmds/init.rs) — this script always pre-writes config.yaml before init.
#   - `localdb --json --store <name> search "<query>" --limit N` prints
#     `{"citations": [...]}` with full Citation objects (chunk_id, uri, score.fused/
#     dense/bm25, ...) — see cli/src/cmds/search.rs print_search_output() and
#     core/src/citation.rs. chunk_id is content-addressed (blake3 of chunk text), so it
#     is identical across embedding providers for the same input — used as the parity
#     key in step 5.
#   - Logging: localdb/src/main.rs initializes tracing_subscriber with
#     EnvFilter::try_from_default_env(), i.e. RUST_LOG (default var name; there is no
#     LOCALDB_LOG). RUST_LOG=debug is what this script sets for index/search calls.
#
# Exit codes surfaced by localdb itself (specs/05-surfaces.md §5, stable API): 0 ok,
# 1 internal, 2 invalid usage/config, 3 not found, 4 conflict/locked, 5 unavailable.
# This script treats any non-zero exit from a localdb invocation as a step FAILure.
#
# VALIDATION PERFORMED WHILE WRITING THIS SCRIPT (on a GPU-less macOS dev machine):
#   bash -n scripts/cuda-verify.sh
#   static lint (ShellCheck) run against scripts/cuda-verify.sh
#   ./scripts/cuda-verify.sh --dry-run https://example.invalid/localdb.tar.gz
#
# ---------------------------------------------------------------------------------

set -euo pipefail

# ---- argument parsing -------------------------------------------------------------

TARBALL_URL=""
DRY_RUN=false

for arg in "$@"; do
    case "$arg" in
        --dry-run)
            DRY_RUN=true
            ;;
        -h|--help)
            sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d'
            exit 0
            ;;
        *)
            if [ -z "$TARBALL_URL" ]; then
                TARBALL_URL="$arg"
            fi
            ;;
    esac
done

if [ "$DRY_RUN" = false ] && [ -z "$TARBALL_URL" ]; then
    echo "usage: $0 <release-tarball-url> [--dry-run]" >&2
    exit 2
fi

# ---- report / logging helpers ------------------------------------------------------

REPORT_FILE="$(pwd)/cuda-verify-report.txt"
: > "$REPORT_FILE"

OVERALL_FAIL=0

emit() {
    printf '%s\n' "$*" | tee -a "$REPORT_FILE"
}

section() {
    emit ""
    emit "=== $* ==="
}

pass() {
    emit "[PASS] $*"
}

fail() {
    emit "[FAIL] $*"
    OVERALL_FAIL=1
}

skip() {
    emit "[SKIP] $*"
}

info() {
    emit "[INFO] $*"
}

evidence() {
    # Appends raw evidence text (command output, log excerpts) indented, report-only.
    while IFS= read -r line; do
        emit "    | $line"
    done
}

emit "cuda-verify.sh report — $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
emit "dry-run: $DRY_RUN"
[ -n "$TARBALL_URL" ] && emit "tarball: $TARBALL_URL"

# ---- Step 1: environment evidence --------------------------------------------------

section "Step 1: environment evidence"

NVIDIA_SMI_HEADER=""
if command -v nvidia-smi >/dev/null 2>&1; then
    NVIDIA_SMI_HEADER="$(nvidia-smi 2>&1 | head -n 12 || true)"
    if [ -n "$NVIDIA_SMI_HEADER" ]; then
        pass "nvidia-smi present"
        printf '%s\n' "$NVIDIA_SMI_HEADER" | evidence
    else
        if [ "$DRY_RUN" = true ]; then
            skip "nvidia-smi produced no output"
        else
            fail "nvidia-smi produced no output"
        fi
    fi
else
    if [ "$DRY_RUN" = true ]; then
        skip "nvidia-smi not found (expected on a GPU-less dry-run machine)"
    else
        fail "nvidia-smi not found — no NVIDIA driver installed?"
    fi
fi

DRIVER_VERSION="$(printf '%s' "$NVIDIA_SMI_HEADER" | grep -oE 'Driver Version: [0-9.]+' | head -1 || true)"
CUDA_VERSION_LINE="$(printf '%s' "$NVIDIA_SMI_HEADER" | grep -oE 'CUDA Version: [0-9.]+' | head -1 || true)"
[ -n "$DRIVER_VERSION" ] && info "$DRIVER_VERSION"
[ -n "$CUDA_VERSION_LINE" ] && info "$CUDA_VERSION_LINE"

if command -v ldconfig >/dev/null 2>&1; then
    LDCONFIG_MATCHES="$(ldconfig -p 2>/dev/null | grep -E 'libcudnn\.so\.9|libcudart\.so\.12' || true)"
    if [ -n "$LDCONFIG_MATCHES" ]; then
        pass "ldconfig lists libcudnn.so.9 / libcudart.so.12"
        printf '%s\n' "$LDCONFIG_MATCHES" | evidence
    else
        if [ "$DRY_RUN" = true ]; then
            skip "ldconfig found no libcudnn.so.9 / libcudart.so.12 entries"
        else
            fail "ldconfig found no libcudnn.so.9 / libcudart.so.12 entries — CUDA runtime/cuDNN missing?"
        fi
    fi
else
    if [ "$DRY_RUN" = true ]; then
        skip "ldconfig not found (expected on non-Linux dry-run machine)"
    else
        fail "ldconfig not found"
    fi
fi

UNAME_OUT="$(uname -a)"
info "uname -a: $UNAME_OUT"

GLIBC_VERSION=""
if command -v ldd >/dev/null 2>&1; then
    GLIBC_VERSION="$(ldd --version 2>&1 | head -1 || true)"
elif command -v getconf >/dev/null 2>&1 && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
    GLIBC_VERSION="$(getconf GNU_LIBC_VERSION)"
fi
if [ -n "$GLIBC_VERSION" ]; then
    info "glibc: $GLIBC_VERSION"
else
    skip "glibc version unavailable (non-Linux host?)"
fi

# ---- dry-run short-circuit ---------------------------------------------------------

if [ "$DRY_RUN" = true ]; then
    section "Steps 2-6: skipped (--dry-run)"
    skip "setup (tarball download/extract)"
    skip "forced CUDA run"
    skip "CPU reference run"
    skip "parity check"
    skip "auto-detection run"

    section "Summary"
    info "hardware: ${DRIVER_VERSION:-<none>} ${CUDA_VERSION_LINE:-<none>}"
    info "date: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    if [ "$OVERALL_FAIL" -eq 0 ]; then
        pass "dry-run complete — no FAILs in environment evidence"
        emit ""
        emit "OVERALL: PASS (dry-run)"
        exit 0
    else
        fail "dry-run complete — environment evidence step reported failures above"
        emit ""
        emit "OVERALL: FAIL (dry-run)"
        exit 1
    fi
fi

# ---- Step 2: setup ------------------------------------------------------------------

section "Step 2: setup"

echo "This run downloads approximately 900 MB total:" >&2
echo "  - release tarball (a few MB)"                   >&2
echo "  - embedding model 'pplx-embed-context-v1-0.6b' (~706 MB, one-time, cached)" >&2
echo "  - CUDA-flavored ONNX Runtime (~196 MB, one-time, cached)"                    >&2

WORKDIR="$(mktemp -d -t cuda-verify-XXXXXX)"
info "workdir: $WORKDIR"
trap 'true' EXIT  # deliberately do NOT auto-delete; sizes are reported below, friend can inspect/clean

TARBALL_PATH="$WORKDIR/release.tar.gz"
EXTRACT_DIR="$WORKDIR/extracted"
mkdir -p "$EXTRACT_DIR"

info "downloading tarball from $TARBALL_URL"
if curl -fL --progress-bar "$TARBALL_URL" -o "$TARBALL_PATH"; then
    pass "tarball downloaded ($(du -h "$TARBALL_PATH" | awk '{print $1}'))"
else
    fail "tarball download failed"
    section "Summary"
    emit "OVERALL: FAIL (setup)"
    exit 1
fi

if tar -xzf "$TARBALL_PATH" -C "$EXTRACT_DIR"; then
    pass "tarball extracted"
else
    fail "tarball extraction failed"
    section "Summary"
    emit "OVERALL: FAIL (setup)"
    exit 1
fi

LOCALDB_BIN="$(find "$EXTRACT_DIR" -type f -name 'localdb' -print -quit)"
if [ -z "$LOCALDB_BIN" ]; then
    fail "no 'localdb' binary found inside extracted tarball"
    section "Summary"
    emit "OVERALL: FAIL (setup)"
    exit 1
fi
chmod +x "$LOCALDB_BIN"
pass "localdb binary located: $LOCALDB_BIN"

VERSION_OUT="$("$LOCALDB_BIN" --version 2>&1 || true)"
info "--version: $VERSION_OUT"

# Isolate ALL localdb state under the workdir. dirs::cache_dir()/data_dir()/config_dir()
# on Linux honor XDG_CACHE_HOME / XDG_DATA_HOME / XDG_CONFIG_HOME first (see
# embed/src/model_cache.rs and embed/src/ort_runtime.rs for the cache-dir consumers);
# XDG_STATE_HOME covers the logs dir per specs/03-config.md §4. HOME itself is also
# redirected as a defense-in-depth fallback in case anything (this shell, curl, etc.)
# falls back to ~ directly.
export XDG_CACHE_HOME="$WORKDIR/cache"
export XDG_DATA_HOME="$WORKDIR/data"
export XDG_CONFIG_HOME="$WORKDIR/config"
export XDG_STATE_HOME="$WORKDIR/state"
export HOME="$WORKDIR/home"
mkdir -p "$XDG_CACHE_HOME" "$XDG_DATA_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME" "$HOME"
info "XDG_CACHE_HOME=$XDG_CACHE_HOME"
info "XDG_DATA_HOME=$XDG_DATA_HOME"
info "XDG_CONFIG_HOME=$XDG_CONFIG_HOME"
info "XDG_STATE_HOME=$XDG_STATE_HOME"
info "HOME=$HOME"

# ---- fixture documents ---------------------------------------------------------

FIXTURES_DIR="$WORKDIR/fixtures"
mkdir -p "$FIXTURES_DIR"

write_fixture() {
    local name="$1"
    local title="$2"
    local body="$3"
    cat > "$FIXTURES_DIR/$name" <<EOF
# $title

$body

This document is part of the cuda-verify.sh fixture set used to sanity-check
localdb's hybrid search across execution providers.
EOF
}

write_fixture "doc-01-zephyrus.md" "Zephyrus Nebular Cartography Expedition" \
"The Zephyrus nebular cartography expedition mapped drifting gas filaments over
eleven survey passes, cross-referencing spectral cartography with prior nebular
charts."

write_fixture "doc-02-quokka.md" "Marmalade Quokka Botanical Society Records" \
"The marmalade quokka botanical society kept meticulous records of orchard
grafting experiments, cataloguing each quokka-adjacent enclosure's soil pH."

write_fixture "doc-03-obsidian.md" "Obsidian Lighthouse Telemetry Array Readings" \
"The obsidian lighthouse telemetry array streamed hourly readings of tidal
drift, beacon lumens, and fog-horn duty cycle back to the harbor office."

write_fixture "doc-04-tundra.md" "Tundra Glacier Bioacoustics Survey" \
"Researchers deployed hydrophones across the tundra glacier bioacoustics
survey grid to record subglacial meltwater percussion through the season."

write_fixture "doc-05-clockwork.md" "Clockwork Octopus Horology Guild" \
"The clockwork octopus horology guild restores brass automata whose tentacle
gears once drove tidal clocks in coastal observatories."

write_fixture "doc-06-saffron.md" "Saffron Paper Airship Logistics" \
"Saffron paper airship logistics coordinated lightweight cargo runs between
terraced hill farms, favoring dawn winds over the saffron valley."

write_fixture "doc-07-velvet.md" "Velvet Meteor Orchestra Archive" \
"The velvet meteor orchestra archive preserves recordings made during three
consecutive meteor showers, each scored for strings and theremin."

write_fixture "doc-08-copper.md" "Copper Beetle Cartridge Assembly" \
"Copper beetle cartridge assembly lines stamped decorative beetle-shaped
cartridge casings for ceremonial (non-functional) collector rifles."

write_fixture "doc-09-indigo.md" "Indigo Marsh Telemetry Buoy Network" \
"The indigo marsh telemetry buoy network reports salinity and egret nesting
density across the delta's indigo-stained mudflats."

write_fixture "doc-10-pumpkin.md" "Pumpkin Lattice Cryptography Notes" \
"Pumpkin lattice cryptography notes sketch a playful (non-cryptographically
secure) lattice scheme themed around autumn pumpkin harvest festivals."

pass "10 fixture documents written to $FIXTURES_DIR"

QUERY_1="zephyrus nebular cartography expedition"
QUERY_2="marmalade quokka botanical society records"
QUERY_3="obsidian lighthouse telemetry array readings"

# ---- shared run helpers --------------------------------------------------------

# run_localdb <label> <config-file> <out-file> <err-file> -- <args...>
# Runs $LOCALDB_BIN with LOCALDB_CONFIG=<config-file>, capturing stdout/stderr to
# files without letting `set -e` abort the script on a non-zero exit.
run_localdb() {
    local label="$1"; shift
    local config_file="$1"; shift
    local out_file="$1"; shift
    local err_file="$1"; shift
    if [ "$1" = "--" ]; then shift; fi

    local rc=0
    LOCALDB_CONFIG="$config_file" "$LOCALDB_BIN" "$@" >"$out_file" 2>"$err_file" || rc=$?
    if [ "$rc" -eq 0 ]; then
        pass "$label (exit 0)"
    else
        fail "$label (exit $rc)"
        tail -n 20 "$err_file" | evidence
    fi
    return "$rc"
}

write_run_config() {
    local config_file="$1"
    local data_dir="$2"
    local provider="$3"
    mkdir -p "$data_dir"
    cat > "$config_file" <<EOF
version: 1
paths:
  data: $data_dir
defaults:
  indexing:
    embedding:
      provider: $provider
EOF
}

# ---- Step 3: forced CUDA run ----------------------------------------------------

section "Step 3: forced CUDA run (provider: local-cuda)"

CUDA_RUN_DIR="$WORKDIR/run-cuda"
CUDA_CONFIG="$CUDA_RUN_DIR/config.yaml"
write_run_config "$CUDA_CONFIG" "$CUDA_RUN_DIR/data" "local-cuda"

CUDA_INDEX_ERR="$CUDA_RUN_DIR/index.stderr.log"
CUDA_OK=1

run_localdb "cuda: init" "$CUDA_CONFIG" "$CUDA_RUN_DIR/init.out" "$CUDA_RUN_DIR/init.err" -- init || CUDA_OK=0
run_localdb "cuda: store add" "$CUDA_CONFIG" "$CUDA_RUN_DIR/store.out" "$CUDA_RUN_DIR/store.err" -- store add cuda-store || CUDA_OK=0
run_localdb "cuda: source add" "$CUDA_CONFIG" "$CUDA_RUN_DIR/source.out" "$CUDA_RUN_DIR/source.err" -- --store cuda-store source add "$FIXTURES_DIR" || CUDA_OK=0

RUST_LOG=debug run_localdb "cuda: index" "$CUDA_CONFIG" "$CUDA_RUN_DIR/index.out" "$CUDA_INDEX_ERR" -- --store cuda-store index --strict || CUDA_OK=0

if grep -qF "downloading ONNX Runtime" "$CUDA_INDEX_ERR"; then
    pass "cuda: ORT download log line observed ('downloading ONNX Runtime')"
else
    info "cuda: no ORT download log line (cache may have been warm already)"
fi

CUDA_ORT_CACHE_DIR="$XDG_CACHE_HOME/localdb/ort/1.24.4-cuda"
if [ -d "$CUDA_ORT_CACHE_DIR" ] && [ -n "$(ls -A "$CUDA_ORT_CACHE_DIR" 2>/dev/null)" ]; then
    pass "cuda: ORT CUDA cache dir populated ($CUDA_ORT_CACHE_DIR)"
    find "$CUDA_ORT_CACHE_DIR" -maxdepth 1 -type f -exec ls -la {} + | evidence
else
    fail "cuda: expected ORT CUDA cache dir missing/empty ($CUDA_ORT_CACHE_DIR)"
    CUDA_OK=0
fi

if grep -qF 'cache_subdir="1.24.4-cuda"' "$CUDA_INDEX_ERR" || grep -qF "cache_subdir=1.24.4-cuda" "$CUDA_INDEX_ERR"; then
    pass "cuda: 'initializing ONNX Runtime (downloaded)' logged with cache_subdir=1.24.4-cuda (CUDA flavor committed)"
else
    fail "cuda: no log evidence the CUDA-flavored ONNX Runtime was committed"
    CUDA_OK=0
fi

if grep -qF "CUDA execution provider unavailable" "$CUDA_INDEX_ERR"; then
    fail "cuda: 'CUDA execution provider unavailable' appeared in index stderr"
    CUDA_OK=0
else
    pass "cuda: no 'CUDA execution provider unavailable' error in index stderr"
fi

CUDA_SEARCH_1="$CUDA_RUN_DIR/search-1.json"
CUDA_SEARCH_2="$CUDA_RUN_DIR/search-2.json"
CUDA_SEARCH_3="$CUDA_RUN_DIR/search-3.json"

RUST_LOG=debug run_localdb "cuda: search 1" "$CUDA_CONFIG" "$CUDA_SEARCH_1" "$CUDA_RUN_DIR/search-1.err" -- --json --store cuda-store search "$QUERY_1" --limit 5 || CUDA_OK=0
RUST_LOG=debug run_localdb "cuda: search 2" "$CUDA_CONFIG" "$CUDA_SEARCH_2" "$CUDA_RUN_DIR/search-2.err" -- --json --store cuda-store search "$QUERY_2" --limit 5 || CUDA_OK=0
RUST_LOG=debug run_localdb "cuda: search 3" "$CUDA_CONFIG" "$CUDA_SEARCH_3" "$CUDA_RUN_DIR/search-3.err" -- --json --store cuda-store search "$QUERY_3" --limit 5 || CUDA_OK=0

for f in "$CUDA_RUN_DIR"/search-*.err; do
    if grep -qF "CUDA execution provider unavailable" "$f" 2>/dev/null; then
        fail "cuda: 'CUDA execution provider unavailable' appeared in $(basename "$f")"
        CUDA_OK=0
    fi
done

# ---- Step 4: CPU reference run --------------------------------------------------

section "Step 4: CPU reference run (provider: local-onnx)"

CPU_RUN_DIR="$WORKDIR/run-cpu"
CPU_CONFIG="$CPU_RUN_DIR/config.yaml"
write_run_config "$CPU_CONFIG" "$CPU_RUN_DIR/data" "local-onnx"

CPU_OK=1

run_localdb "cpu: init" "$CPU_CONFIG" "$CPU_RUN_DIR/init.out" "$CPU_RUN_DIR/init.err" -- init || CPU_OK=0
run_localdb "cpu: store add" "$CPU_CONFIG" "$CPU_RUN_DIR/store.out" "$CPU_RUN_DIR/store.err" -- store add cpu-store || CPU_OK=0
run_localdb "cpu: source add" "$CPU_CONFIG" "$CPU_RUN_DIR/source.out" "$CPU_RUN_DIR/source.err" -- --store cpu-store source add "$FIXTURES_DIR" || CPU_OK=0
run_localdb "cpu: index" "$CPU_CONFIG" "$CPU_RUN_DIR/index.out" "$CPU_RUN_DIR/index.err" -- --store cpu-store index --strict || CPU_OK=0

CPU_SEARCH_1="$CPU_RUN_DIR/search-1.json"
CPU_SEARCH_2="$CPU_RUN_DIR/search-2.json"
CPU_SEARCH_3="$CPU_RUN_DIR/search-3.json"

run_localdb "cpu: search 1" "$CPU_CONFIG" "$CPU_SEARCH_1" "$CPU_RUN_DIR/search-1.err" -- --json --store cpu-store search "$QUERY_1" --limit 5 || CPU_OK=0
run_localdb "cpu: search 2" "$CPU_CONFIG" "$CPU_SEARCH_2" "$CPU_RUN_DIR/search-2.err" -- --json --store cpu-store search "$QUERY_2" --limit 5 || CPU_OK=0
run_localdb "cpu: search 3" "$CPU_CONFIG" "$CPU_SEARCH_3" "$CPU_RUN_DIR/search-3.err" -- --json --store cpu-store search "$QUERY_3" --limit 5 || CPU_OK=0

# ---- Step 5: parity check -------------------------------------------------------

section "Step 5: CUDA / CPU parity check (top-5 chunk_id overlap)"

HAVE_JQ=false
command -v jq >/dev/null 2>&1 && HAVE_JQ=true

top5_chunk_ids() {
    # Prints one chunk_id per line, in ranked order, from a --json search result file.
    local file="$1"
    if [ "$HAVE_JQ" = true ]; then
        jq -r '.citations[0:5][].chunk_id' "$file" 2>/dev/null
    else
        # Fallback: serde_json::to_string_pretty puts one field per line, so a plain
        # grep -o preserves array order. Limit to the first 5 matches (top-5).
        grep -o '"chunk_id": *"[^"]*"' "$file" | sed -E 's/.*"([^"]+)"$/\1/' | head -n 5
    fi
}

parity_ok=1
compare_query() {
    local label="$1" cuda_file="$2" cpu_file="$3"

    if [ ! -s "$cuda_file" ] || [ ! -s "$cpu_file" ]; then
        fail "parity ($label): missing search output file(s)"
        parity_ok=0
        return
    fi

    local cuda_ids cpu_ids
    cuda_ids="$(top5_chunk_ids "$cuda_file")"
    cpu_ids="$(top5_chunk_ids "$cpu_file")"

    if [ -z "$cuda_ids" ] || [ -z "$cpu_ids" ]; then
        fail "parity ($label): no citations returned on one or both sides"
        parity_ok=0
        return
    fi

    local cuda_top1 cpu_top1
    cuda_top1="$(printf '%s\n' "$cuda_ids" | head -1)"
    cpu_top1="$(printf '%s\n' "$cpu_ids" | head -1)"

    local n overlap required
    n="$(printf '%s\n' "$cuda_ids" | wc -l | tr -d ' ')"
    overlap="$(comm -12 <(printf '%s\n' "$cuda_ids" | sort) <(printf '%s\n' "$cpu_ids" | sort) | wc -l | tr -d ' ')"
    if [ "$n" -ge 5 ]; then
        required=4
    else
        required="$n"
    fi

    if [ "$cuda_top1" = "$cpu_top1" ] && [ "$overlap" -ge "$required" ]; then
        pass "parity ($label): top-1 identical, overlap $overlap/$n (required >= $required)"
    else
        fail "parity ($label): top-1 match=$([ "$cuda_top1" = "$cpu_top1" ] && echo yes || echo no), overlap $overlap/$n (required >= $required)"
        info "parity ($label): cuda top-1=$cuda_top1 cpu top-1=$cpu_top1"
        parity_ok=0
    fi
}

compare_query "query 1: $QUERY_1" "$CUDA_SEARCH_1" "$CPU_SEARCH_1"
compare_query "query 2: $QUERY_2" "$CUDA_SEARCH_2" "$CPU_SEARCH_2"
compare_query "query 3: $QUERY_3" "$CUDA_SEARCH_3" "$CPU_SEARCH_3"

# ---- Step 6: auto-detection run -------------------------------------------------

section "Step 6: auto-detection run (provider: local)"

AUTO_RUN_DIR="$WORKDIR/run-auto"
AUTO_CONFIG="$AUTO_RUN_DIR/config.yaml"
write_run_config "$AUTO_CONFIG" "$AUTO_RUN_DIR/data" "local"

AUTO_INDEX_ERR="$AUTO_RUN_DIR/index.stderr.log"
AUTO_OK=1

run_localdb "auto: init" "$AUTO_CONFIG" "$AUTO_RUN_DIR/init.out" "$AUTO_RUN_DIR/init.err" -- init || AUTO_OK=0
run_localdb "auto: store add" "$AUTO_CONFIG" "$AUTO_RUN_DIR/store.out" "$AUTO_RUN_DIR/store.err" -- store add auto-store || AUTO_OK=0
run_localdb "auto: source add" "$AUTO_CONFIG" "$AUTO_RUN_DIR/source.out" "$AUTO_RUN_DIR/source.err" -- --store auto-store source add "$FIXTURES_DIR" || AUTO_OK=0
RUST_LOG=debug run_localdb "auto: index" "$AUTO_CONFIG" "$AUTO_RUN_DIR/index.out" "$AUTO_INDEX_ERR" -- --store auto-store index --strict || AUTO_OK=0

if grep -qF "NVIDIA/CUDA stack detected; using CUDA-enabled ONNX Runtime" "$AUTO_INDEX_ERR"; then
    pass "auto: factory chose CUDA automatically ('NVIDIA/CUDA stack detected; using CUDA-enabled ONNX Runtime')"
else
    fail "auto: expected 'NVIDIA/CUDA stack detected' info line not found — automatic mode did not select CUDA"
    AUTO_OK=0
    if grep -qF "no complete NVIDIA/CUDA stack detected" "$AUTO_INDEX_ERR"; then
        info "auto: found the negative-case line instead — detection ladder did not pass on this machine"
    fi
fi

run_localdb "auto: search" "$AUTO_CONFIG" "$AUTO_RUN_DIR/search-1.json" "$AUTO_RUN_DIR/search-1.err" -- --json --store auto-store search "$QUERY_1" --limit 5 || AUTO_OK=0

# ---- Step 7: report -----------------------------------------------------------

section "Step 7: disk usage (for cleanup / attribution)"
info "model + ORT cache ($XDG_CACHE_HOME): $(du -sh "$XDG_CACHE_HOME" 2>/dev/null | awk '{print $1}')"
info "store data ($XDG_DATA_HOME + run dirs): $(du -sh "$WORKDIR" 2>/dev/null | awk '{print $1}') (whole workdir)"
info "workdir (not auto-deleted, remove manually when done): $WORKDIR"

section "Summary"
info "hardware: ${DRIVER_VERSION:-<none>} ${CUDA_VERSION_LINE:-<none>}"
info "uname: $UNAME_OUT"
info "date: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
info "step 3 (forced CUDA run): $([ "$CUDA_OK" -eq 1 ] && echo PASS || echo FAIL)"
info "step 4 (CPU reference run): $([ "$CPU_OK" -eq 1 ] && echo PASS || echo FAIL)"
info "step 5 (parity check): $([ "$parity_ok" -eq 1 ] && echo PASS || echo FAIL)"
info "step 6 (auto-detection run): $([ "$AUTO_OK" -eq 1 ] && echo PASS || echo FAIL)"

if [ "$OVERALL_FAIL" -eq 0 ] && [ "$CUDA_OK" -eq 1 ] && [ "$CPU_OK" -eq 1 ] \
    && [ "$parity_ok" -eq 1 ] && [ "$AUTO_OK" -eq 1 ]; then
    emit ""
    emit "OVERALL: PASS"
    emit "Report written to: $REPORT_FILE"
    exit 0
else
    emit ""
    emit "OVERALL: FAIL"
    emit "Report written to: $REPORT_FILE"
    exit 1
fi
