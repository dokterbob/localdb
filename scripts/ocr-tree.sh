#!/usr/bin/env bash
#
# ocr-tree.sh — batch OCR a directory tree of PDFs with ocrmypdf
#
# ---------------------------------------------------------------------------------
# USAGE
#
#   scripts/ocr-tree.sh <input_dir> <output_dir> [-- <extra ocrmypdf args>...]
#
#   <input_dir>    Directory to walk recursively for *.pdf files (case-insensitive).
#   <output_dir>   Directory to mirror the input tree into. Created if missing.
#
#   -- <extra ...>  Optional. Everything after a literal `--` is passed through
#                   verbatim to every ocrmypdf invocation, after this script's
#                   own default arguments. Use this to enable OCR engine plugins
#                   (e.g. --plugin ocrmypdf_macocr, --plugin ocrmypdf_paddleocr)
#                   or to override the default text-skip behavior (see below).
#
# DEFAULT BEHAVIOR
#
#   - By default this script appends `--skip-text` to every ocrmypdf call, so
#     pages that already have an extractable text layer are left alone instead
#     of erroring out. If your passthrough args already select a mode
#     (--skip-text, --force-ocr, --redo-ocr, or --mode), the default is NOT
#     added — ocrmypdf treats those as mutually exclusive and hard-errors if
#     more than one is given.
#   - Execution is sequential (no parallelism) and resumable: if the mirrored
#     output file already exists, it is skipped without re-invoking ocrmypdf.
#     Re-running this script after a partial/failed batch will only retry the
#     files that don't yet have output.
#   - A single file's OCR failure is logged and does not abort the batch; the
#     script keeps going and reports a summary (and non-zero exit) at the end.
#
# ASSUMPTIONS
#
#   - `ocrmypdf` is on PATH.
#   - Output paths mirror input paths 1:1, relative to <input_dir>, rooted at
#     <output_dir>, with the same relative subdirectories and filenames.
#
# EXIT CODES
#
#   0   all discovered files were OCR'd or skipped (already had output); zero
#       failures
#   1   the batch completed but at least one file failed OCR
#   2   usage/setup error (bad args, missing input dir, ocrmypdf not found)
#
# ---------------------------------------------------------------------------------

set -euo pipefail

# ---- helpers ----------------------------------------------------------------

info()  { printf '\033[1;34m[ocr-tree]\033[0m %s\n' "$*"; }
ok()    { printf '\033[1;32m[ocr-tree] OK:\033[0m %s\n' "$*"; }
warn()  { printf '\033[1;33m[ocr-tree] WARN:\033[0m %s\n' "$*"; }
err()   { printf '\033[1;31m[ocr-tree] ERROR:\033[0m %s\n' "$*" >&2; }

usage() {
    sed -n '2,/^set -euo pipefail$/p' "$0" | sed 's/^# \{0,1\}//' | sed '$d'
}

exit_code_desc() {
    case "$1" in
        1)  echo "bad arguments" ;;
        2)  echo "input file error" ;;
        3)  echo "missing dependency" ;;
        4)  echo "invalid output PDF" ;;
        5)  echo "file access error" ;;
        6)  echo "already has OCR text" ;;
        7)  echo "child process error" ;;
        8)  echo "encrypted PDF" ;;
        9)  echo "invalid configuration" ;;
        10) echo "PDF/A conversion failed" ;;
        15) echo "other error" ;;
        130) echo "interrupted" ;;
        *)  echo "unknown error" ;;
    esac
}

has_mode_flag() {
    if [ "${#EXTRA_ARGS[@]}" -eq 0 ]; then
        return 1
    fi
    local arg
    for arg in "${EXTRA_ARGS[@]}"; do
        case "$arg" in
            --skip-text|--force-ocr|--redo-ocr|--mode|--mode=*)
                return 0
                ;;
        esac
    done
    return 1
}

# ---- argument parsing --------------------------------------------------------

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

if [ "$#" -lt 2 ]; then
    usage
    exit 2
fi

INPUT_DIR="$1"
OUTPUT_DIR="$2"
shift 2

EXTRA_ARGS=()
if [ "$#" -gt 0 ]; then
    if [ "$1" != "--" ]; then
        err "expected '--' before passthrough ocrmypdf args, got: $1"
        usage
        exit 2
    fi
    shift
    EXTRA_ARGS=("$@")
fi

if [ ! -d "$INPUT_DIR" ]; then
    err "input directory does not exist: $INPUT_DIR"
    exit 2
fi

if ! command -v ocrmypdf &>/dev/null; then
    err "ocrmypdf not found on PATH"
    exit 2
fi

mkdir -p "$OUTPUT_DIR" || { err "failed to create output directory: $OUTPUT_DIR"; exit 2; }

# ---- resolve absolute paths --------------------------------------------------

INPUT_DIR="$(cd "$INPUT_DIR" && pwd)"
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"

info "Input:  $INPUT_DIR"
info "Output: $OUTPUT_DIR"

OCR_BASE_ARGS=()
if [ "${#EXTRA_ARGS[@]}" -gt 0 ]; then
    OCR_BASE_ARGS+=("${EXTRA_ARGS[@]}")
fi
if ! has_mode_flag; then
    OCR_BASE_ARGS+=(--skip-text)
fi

info "ocrmypdf args: ${OCR_BASE_ARGS[*]:-<none>}"

# ---- walk the tree -----------------------------------------------------------

FOUND=0
PROCESSED=0
SKIPPED=0
FAILED=0
FAILED_PATHS=()

INPUT_DIR_LEN=${#INPUT_DIR}

while IFS= read -r -d '' src; do
    FOUND=$((FOUND + 1))

    rel="${src:INPUT_DIR_LEN+1}"
    dst="$OUTPUT_DIR/$rel"
    dst_dir=$(dirname "$dst")

    if [ -f "$dst" ]; then
        info "[skip] already exists: $rel"
        SKIPPED=$((SKIPPED + 1))
        continue
    fi

    mkdir -p "$dst_dir"

    info "[ocr]  $rel"
    if ocrmypdf "${OCR_BASE_ARGS[@]}" "$src" "$dst"; then
        ok "$rel"
        PROCESSED=$((PROCESSED + 1))
    else
        rc=$?
        desc=$(exit_code_desc "$rc")
        warn "$rel (exit $rc: $desc)"
        FAILED=$((FAILED + 1))
        FAILED_PATHS+=("$rel (exit $rc: $desc)")
        rm -f "$dst"
    fi
done < <(find "$INPUT_DIR" -type f -iname '*.pdf' -print0)

# ---- summary ------------------------------------------------------------------

info "----------------------------------------"
info "Found:     $FOUND"
info "Processed: $PROCESSED"
info "Skipped:   $SKIPPED"
info "Failed:    $FAILED"

if [ "$FAILED" -gt 0 ]; then
    warn "Failed files:"
    for f in "${FAILED_PATHS[@]}"; do
        warn "  $f"
    done
    exit 1
fi

ok "All files OCR'd or skipped."
exit 0
