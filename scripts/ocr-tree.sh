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
#                   (e.g. --plugin ocrmypdf_appleocr, --plugin ocrmypdf_paddleocr)
#                   or to override the default text-skip behavior (see below).
#
# DEFAULT BEHAVIOR
#
#   - In the default mode (no --force-ocr/--redo-ocr/--mode in your passthrough
#     args), a file is skipped ENTIRELY — no ocrmypdf invocation, no output
#     written — when it already has a real text layer. This is decided by
#     sampling the first few pages with `pdftotext`/`pdfinfo` (poppler) before
#     ocrmypdf ever runs, so already-text-bearing books aren't needlessly
#     rewritten (ocrmypdf's own `--skip-text` only skips OCR per PAGE — it
#     still runs the full pipeline, incl. Ghostscript PDF/A conversion, on
#     every file it's given). `--skip-text` is still appended to the ocrmypdf
#     call as a second line of defense, for files with a mix of text and
#     scanned pages that pass the whole-file sample check.
#   - If your passthrough args already select a mode (--skip-text, --force-ocr,
#     --redo-ocr, or --mode), the text-layer pre-check above is skipped
#     entirely and every discovered file is handed to ocrmypdf — this is how
#     you force reprocessing of files that already have text (e.g. --force-ocr
#     to add a better OCR layer on top of an existing one). The script does
#     NOT add --skip-text on top of an explicit mode flag — ocrmypdf treats
#     --skip-text/--force-ocr/--redo-ocr/--mode as mutually exclusive and
#     hard-errors if more than one is given.
#   - Execution is sequential (no parallelism) and resumable: if the mirrored
#     output file already exists, it is skipped without re-invoking ocrmypdf.
#     Re-running this script after a partial/failed/interrupted batch will
#     only retry the files that don't yet have output.
#   - A single file's OCR failure is logged and does not abort the batch; the
#     script keeps going and reports a summary (and non-zero exit) at the end.
#   - Ctrl-C (SIGINT) stops the batch, not just the current file: the OS
#     delivers it to the running ocrmypdf directly (it shares this script's
#     foreground process group), that file is logged as failed (interrupted),
#     a summary is printed, and the script exits 130 without starting any
#     further files — it does not silently move on to the next book.
#     Re-running afterwards resumes via the skip-existing-output behavior
#     above. Note ocrmypdf itself can take a little while to unwind after
#     SIGINT when it has several OCR workers mid-page — that delay is
#     upstream ocrmypdf/multiprocessing behavior, not a hang in this script.
#
# ASSUMPTIONS
#
#   - `ocrmypdf` and poppler's `pdftotext`/`pdfinfo` are on PATH.
#   - Output paths mirror input paths 1:1, relative to <input_dir>, rooted at
#     <output_dir>, with the same relative subdirectories and filenames.
#
# EXIT CODES
#
#   0   all discovered files were OCR'd or skipped (already had output, or
#       already had a text layer); zero failures
#   1   the batch completed but at least one file failed OCR
#   2   usage/setup error (bad args, missing input dir, ocrmypdf/pdftotext not
#       found)
#
# ---------------------------------------------------------------------------------

set -euo pipefail

# ---- tunables -----------------------------------------------------------------

# How many pages (from the start of the document) to sample when deciding
# whether a file already has a usable text layer, and how much extracted
# text (non-whitespace characters, averaged over the sampled pages) counts as
# "has text". Sampling instead of extracting the whole document keeps the
# check fast even for large scanned books.
TEXT_CHECK_SAMPLE_PAGES=5
TEXT_CHECK_MIN_CHARS_PER_PAGE=100

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

# Sample the first few pages of a PDF and decide whether it already has a
# usable text layer. Returns success (0) if it does, failure (1) if it looks
# scanned/textless (or if it couldn't be inspected, e.g. corrupt PDF — in
# which case ocrmypdf is left to fail on it with a proper diagnostic).
has_text_layer() {
    local pdf="$1"
    local pages
    pages=$(pdfinfo "$pdf" 2>/dev/null | awk -F': *' '/^Pages:/ {print $2}')
    case "$pages" in
        ''|*[!0-9]*) return 1 ;;
    esac
    if [ "$pages" -eq 0 ]; then
        return 1
    fi
    local sample=$TEXT_CHECK_SAMPLE_PAGES
    if [ "$pages" -lt "$sample" ]; then
        sample=$pages
    fi
    local text stripped chars avg
    text=$(pdftotext -f 1 -l "$sample" "$pdf" - 2>/dev/null || true)
    stripped=$(printf '%s' "$text" | tr -d '[:space:]')
    chars=${#stripped}
    avg=$((chars / sample))
    [ "$avg" -ge "$TEXT_CHECK_MIN_CHARS_PER_PAGE" ]
}

print_summary() {
    info "----------------------------------------"
    info "Found:     $FOUND"
    info "Processed: $PROCESSED"
    info "Skipped:   $((SKIPPED_EXISTING + SKIPPED_TEXT))  (output existed: $SKIPPED_EXISTING, already had text: $SKIPPED_TEXT)"
    info "Failed:    $FAILED"
    if [ "$FAILED" -gt 0 ]; then
        warn "Failed files:"
        for f in "${FAILED_PATHS[@]}"; do
            warn "  $f"
        done
    fi
}

# Fires if SIGINT/SIGTERM arrives while this script is doing something other
# than waiting on ocrmypdf (e.g. mid text-layer check, or between files) —
# the ordinary interrupted-ocrmypdf case is handled inline in the main loop
# below, where the exit-130 branch stops the batch itself. This is a
# best-effort fallback so we still print a summary instead of dying silently.
on_interrupt() {
    trap '' INT TERM
    warn "Interrupted — stopping."
    print_summary
    exit 130
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

if ! command -v pdftotext &>/dev/null || ! command -v pdfinfo &>/dev/null; then
    err "pdftotext/pdfinfo (poppler) not found on PATH — required for the text-layer skip check"
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

SKIP_TEXTED_FILES=0
if ! has_mode_flag; then
    OCR_BASE_ARGS+=(--skip-text)
    SKIP_TEXTED_FILES=1
fi

info "ocrmypdf args: ${OCR_BASE_ARGS[*]:-<none>}"

# ---- walk the tree -----------------------------------------------------------

FOUND=0
PROCESSED=0
SKIPPED_EXISTING=0
SKIPPED_TEXT=0
FAILED=0
FAILED_PATHS=()

trap on_interrupt INT TERM

INPUT_DIR_LEN=${#INPUT_DIR}

while IFS= read -r -d '' src; do
    FOUND=$((FOUND + 1))

    rel="${src:INPUT_DIR_LEN+1}"
    dst="$OUTPUT_DIR/$rel"
    dst_dir=$(dirname "$dst")

    if [ -f "$dst" ]; then
        info "[skip] already exists: $rel"
        SKIPPED_EXISTING=$((SKIPPED_EXISTING + 1))
        continue
    fi

    if [ "$SKIP_TEXTED_FILES" -eq 1 ] && has_text_layer "$src"; then
        info "[skip] already has a text layer: $rel"
        SKIPPED_TEXT=$((SKIPPED_TEXT + 1))
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
        # A real Ctrl-C delivers SIGINT to ocrmypdf directly (it shares this
        # script's foreground process group), which is why bash lets this
        # if/else run to completion instead of aborting outright. Stop the
        # batch here rather than silently starting the next file — resuming
        # later will pick up where this left off via the skip-existing check.
        if [ "$rc" -eq 130 ]; then
            print_summary
            exit 130
        fi
    fi
done < <(find "$INPUT_DIR" -type f -iname '*.pdf' -print0)

# ---- summary ------------------------------------------------------------------

print_summary

if [ "$FAILED" -gt 0 ]; then
    exit 1
fi

ok "All files OCR'd or skipped."
exit 0
