#!/usr/bin/env bash
# check-preamble-budget.sh - CI gate for footnote-owned SessionStart markdown.
#
# Run: bash scripts/ci/check-preamble-budget.sh [--quiet] [repo-root]
# Default root is the current directory. Exits 0 at or below the byte ceiling
# and exits 1 when discovery fails or the measured preamble exceeds it.

set -euo pipefail

# Lowered from 38000 by the 674 bytes the stale `fno test` corpus entry freed:
# a graduated entry banks its saving as a lower ceiling, never as quiet slack.
CEILING_BYTES=37326
RATCHET_NUDGE_BYTES=2000
QUIET=0
REPO_ROOT="."
REPO_ROOT_SET=0

for arg in "$@"; do
  case "$arg" in
    -q|--quiet)
      QUIET=1
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
  TOTAL_BYTES=$((TOTAL_BYTES + bytes))
  RECORDS+="${bytes}"$'\t'"${relative}"$'\n'
done

APPROX_TOKENS=$((TOTAL_BYTES / 4))
SPARE_BYTES=$((CEILING_BYTES - TOTAL_BYTES))
APPROX_TOKEN_K=$(awk -v bytes="$TOTAL_BYTES" 'BEGIN { printf "%.1f", bytes / 4000 }')

if (( QUIET )); then
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
  if (( ! QUIET && SPARE_BYTES > RATCHET_NUDGE_BYTES )); then
    echo "check-preamble-budget: advisory: lower CEILING_BYTES; more than ${RATCHET_NUDGE_BYTES} bytes are unused"
  fi
  exit 0
fi

OVERAGE=$((TOTAL_BYTES - CEILING_BYTES))
OVERAGE_TOKENS=$(((OVERAGE + 3) / 4))

if (( ! QUIET )); then
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
