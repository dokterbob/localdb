#!/usr/bin/env bash
#
# Fetch the non-redistributable PDF(s) used by the extract-crate corpus test
# (extract/tests/corpus.rs) into the gitignored slot in the corpus fixture dir.
#
# Only `sewtha.pdf` (David MacKay, "Sustainable Energy — without the hot air")
# is fetched: it is free to download but carries no redistribution grant, so it
# is not vendored. It is the real-world repro for issue #87 (the old pdf-extract
# parser panicked on it). The corpus test is ignored-if-absent, so running it is
# optional; run this script to exercise the full corpus locally.
#
# The redistributable corpus PDFs are committed under
# extract/tests/fixtures/corpus/ and need no download.
#
# Usage:  scripts/fetch_test_pdfs.sh
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest_dir="$repo_root/extract/tests/fixtures/corpus"
dest="$dest_dir/sewtha-sustainable-energy.pdf"
url="https://www.inference.org.uk/sustainable/book/tex/sewtha.pdf"
sha256="ade22462ddf3f1caa32b2f641b583db7282f95f4b6ef7501d2394a9ddb64745c"

mkdir -p "$dest_dir"

if [ -f "$dest" ]; then
  echo "already present: $dest"
else
  echo "downloading sewtha.pdf (~14 MB) from $url"
  curl -fSL --retry 3 -o "$dest.tmp" "$url"
  mv "$dest.tmp" "$dest"
fi

# Verify integrity (best-effort — a mismatch is a warning, not a hard failure,
# since the upstream file could legitimately be revised).
if command -v shasum >/dev/null 2>&1; then
  got="$(shasum -a 256 "$dest" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$dest" | awk '{print $1}')"
else
  got=""
fi
if [ -n "$got" ] && [ "$got" != "$sha256" ]; then
  echo "warning: sha256 mismatch for $dest" >&2
  echo "  expected $sha256" >&2
  echo "  got      $got" >&2
fi

echo "ready: $dest"
