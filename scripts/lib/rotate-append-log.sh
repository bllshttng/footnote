#!/usr/bin/env bash
# rotate-append-log.sh <path> <max_bytes>
#
# Bound an append-only log: if <path> exceeds <max_bytes>, truncate it in
# place to roughly the last <max_bytes> worth of WHOLE lines (drops the
# leading partial line so JSONL/line validity is preserved). Idempotent,
# a no-op on a missing file or a non-numeric/<=0 cap, and it never deletes
# the file outright.
set -uo pipefail

# Too few arguments is a usage error; a present-but-empty/garbage cap is a
# caller-friendly no-op (the caller defaults the cap, never passes a bad one).
[[ $# -ge 2 ]] || { echo "usage: rotate-append-log.sh <path> <max_bytes>" >&2; exit 2; }
path="$1"
max_bytes="$2"
[[ -n "$path" ]] || exit 0
[[ "$max_bytes" =~ ^[0-9]+$ ]] || exit 0
[[ "$max_bytes" -gt 0 ]] || exit 0
[[ -f "$path" ]] || exit 0

size=$(wc -c < "$path" 2>/dev/null | tr -d '[:space:]')
[[ "$size" =~ ^[0-9]+$ ]] || exit 0
[[ "$size" -gt "$max_bytes" ]] || exit 0

tmp=$(mktemp "${TMPDIR:-/tmp}/rotate-log.XXXXXX") || exit 0
# Keep the last max_bytes, then drop the (probably partial) first line.
tail -c "$max_bytes" "$path" 2>/dev/null | sed '1d' > "$tmp" 2>/dev/null || { rm -f "$tmp"; exit 0; }
# Degenerate guard: a single line larger than the cap empties $tmp; leave the
# file untouched rather than writing an empty or partial-line file.
if [[ -s "$tmp" ]]; then
  cat "$tmp" > "$path" 2>/dev/null || true
fi
rm -f "$tmp"
exit 0
