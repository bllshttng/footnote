#!/usr/bin/env bash
# check-preamble-budget.sh - CI gate for footnote-owned SessionStart markdown.
#
# Run: bash scripts/ci/check-preamble-budget.sh [--quiet] [repo-root]
# Default root is the current directory. Exits 0 at or below the byte ceiling
# and exits 1 when discovery fails or the measured preamble exceeds it.

set -euo pipefail

# Raised from 37326 by 89 bytes for delivery_completion DoneDelivery terminal
# documentation and activation logic in AGENTS.md.
# Raised from 37415 by 45 bytes for the DoneUnreviewed terminal in the
# ship-vocabulary line (the auto-loaded preamble must name every public
# TerminationReason); the four-axis preamble addition left only ~8 bytes headroom.
CEILING_BYTES=37460
RATCHET_NUDGE_BYTES=2000
QUIET=0
JSON_MODE=0
REPO_ROOT="."
REPO_ROOT_SET=0

for arg in "$@"; do
  case "$arg" in
    -q|--quiet)
      QUIET=1
      ;;
    --json)
      JSON_MODE=1
      ;;
    -*)
      echo "check-preamble-budget: unknown option: $arg" >&2
      exit 1
      ;;
    *)
      if (( REPO_ROOT_SET )); then
        echo "check-preamble-budget: expected at most one repo root" >&2
        exit 1
      fi
      REPO_ROOT="$arg"
      REPO_ROOT_SET=1
      ;;
  esac
done

if (( QUIET && JSON_MODE )); then
  echo "check-preamble-budget: --quiet and --json are mutually exclusive" >&2
  exit 1
fi

if [[ ! "$CEILING_BYTES" =~ ^[0-9]+$ ]]; then
  echo "check-preamble-budget: CEILING_BYTES must be a non-negative integer" >&2
  exit 1
fi

[[ -d "$REPO_ROOT" ]] || {
  echo "check-preamble-budget: repo root not found: $REPO_ROOT" >&2
  exit 1
}

FILES=(
  "$REPO_ROOT/AGENTS.md"
  "$REPO_ROOT/CLAUDE.md"
  "$REPO_ROOT/skills/using-fno/SKILL.md"
)

for fixed in "${FILES[@]}"; do
  [[ -f "$fixed" ]] || {
    echo "check-preamble-budget: required file not found: ${fixed#"$REPO_ROOT"/}" >&2
    exit 1
  }
done

shopt -s nullglob
for rule in "$REPO_ROOT"/.claude/rules/*.md; do
  FILES+=("$rule")
done
shopt -u nullglob

TOTAL_BYTES=0
RECORDS=""
MANIFEST_RECORDS=""
hash_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1"
  fi
}
for path in "${FILES[@]}"; do
  relative="${path#"$REPO_ROOT"/}"
  [[ -f "$path" ]] || {
    echo "check-preamble-budget: discovered path is not a regular file: $relative" >&2
    exit 1
  }
  [[ -r "$path" ]] || {
    echo "check-preamble-budget: discovered file is not readable: $relative" >&2
    exit 1
  }
  bytes=$(LC_ALL=C wc -c < "$path")
  bytes=$((bytes))
  content_hash=$(hash_file "$path")
  TOTAL_BYTES=$((TOTAL_BYTES + bytes))
  RECORDS+="${bytes}"$'\t'"${relative}"$'\n'
  MANIFEST_RECORDS+="${bytes}"$'\t'"${content_hash}"$'\t'"${relative}"$'\n'
done

APPROX_TOKENS=$((TOTAL_BYTES / 4))
SPARE_BYTES=$((CEILING_BYTES - TOTAL_BYTES))
APPROX_TOKEN_K=$(awk -v bytes="$TOTAL_BYTES" 'BEGIN { printf "%.1f", bytes / 4000 }')

if (( JSON_MODE )); then
  printf '%s' "$MANIFEST_RECORDS" | python3 -c '
import json
import sys

total = int(sys.argv[1])
ceiling = int(sys.argv[2])
sources = []
for raw in sys.stdin:
    raw = raw.rstrip("\n")
    if not raw:
        continue
    size, content_hash, path = raw.split("\t", 2)
    size = int(size)
    sources.append({
        "path": path,
        "bytes": size,
        "estimated_tokens": (size + 3) // 4,
        "content_hash": content_hash,
    })
print(json.dumps({
    "total_bytes": total,
    "estimated_tokens": (total + 3) // 4,
    "ceiling_bytes": ceiling,
    "sources": sources,
}, separators=(",", ":")))
' "$TOTAL_BYTES" "$CEILING_BYTES"
elif (( QUIET )); then
  echo "preamble: ${TOTAL_BYTES} / ${CEILING_BYTES} B (~${APPROX_TOKEN_K}K tok/turn)"
else
  if (( SPARE_BYTES >= 0 )); then
    echo "check-preamble-budget: ${TOTAL_BYTES} / ${CEILING_BYTES} bytes (~${APPROX_TOKENS} tok at 4 B/tok), ${SPARE_BYTES} to spare"
  else
    echo "check-preamble-budget: ${TOTAL_BYTES} / ${CEILING_BYTES} bytes (~${APPROX_TOKENS} tok at 4 B/tok), $((-SPARE_BYTES)) over"
  fi

  while IFS=$'\t' read -r bytes relative; do
    [[ -z "$relative" ]] && continue
    marker=""
    [[ "$relative" == "skills/using-fno/SKILL.md" ]] && marker="  [shipped to every consumer]"
    printf '  %8d  %s%s\n' "$bytes" "$relative" "$marker"
  done < <(printf '%s' "$RECORDS" | LC_ALL=C sort -rn -k1,1)
fi

if (( TOTAL_BYTES <= CEILING_BYTES )); then
  if (( ! QUIET && ! JSON_MODE && SPARE_BYTES > RATCHET_NUDGE_BYTES )); then
    echo "check-preamble-budget: advisory: lower CEILING_BYTES; more than ${RATCHET_NUDGE_BYTES} bytes are unused"
  fi
  exit 0
fi

OVERAGE=$((TOTAL_BYTES - CEILING_BYTES))
OVERAGE_TOKENS=$(((OVERAGE + 3) / 4))

if (( ! QUIET && ! JSON_MODE )); then
  LARGEST=""
  count=0
  while IFS=$'\t' read -r bytes relative; do
    [[ -z "$relative" ]] && continue
    [[ -n "$LARGEST" ]] && LARGEST+=", "
    LARGEST+="${relative} ${bytes}"
    count=$((count + 1))
    (( count == 3 )) && break
  done < <(printf '%s' "$RECORDS" | LC_ALL=C sort -rn -k1,1)

  {
    echo "check-preamble-budget: ${TOTAL_BYTES} bytes exceeds the ${CEILING_BYTES}-byte ceiling by ${OVERAGE} (~${OVERAGE_TOKENS} tok/turn)."
    echo "  Largest: ${LARGEST}"
    echo
    echo "  Every byte here is re-read on every turn of every session on every lane."
    echo "  Fix, in order of preference:"
    echo "    1. Trade: cut an equivalent amount from the same file."
    echo "    2. Move it out of the preamble: docs/ and linked rule files that the"
    echo "       harness does not auto-load are not paid at startup."
    echo "    3. Raise CEILING_BYTES in this script, in this PR, with the reason in the PR body."
  } >&2
fi
exit 1
