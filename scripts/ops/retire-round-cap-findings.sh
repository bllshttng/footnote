#!/usr/bin/env bash
# retire-round-cap-findings.sh - supersede every node the deleted round-cap
# vent filed.
#
# The review gate used to FILE a capped finding as a backlog node titled
# "review finding filed at round cap: ..." and merge. The vent is deleted:
# at the configured rounds the review obligation is discharged and open
# findings stay in the PR conversation. The nodes it already filed describe
# an act the gate no longer performs, so they are superseded by the node that
# deleted it.
#
# Dry run by default: prints what it would retire. --apply runs one
# `fno backlog supersede` per candidate. Idempotent: a superseded node leaves
# the open set, so a second run retires zero.
#
# Run once from the canonical checkout after the PR merges (the post-merge
# ritual is the home).
set -euo pipefail

FNO_BIN="${FNO:-fno}"
OWNER_NODE="x-0c29"
CAUSE="vented at the round cap; the vent is deleted"
SURFACE="cli/src/fno/pr/_coverage_gate.py"
TITLE_PREFIX="review finding filed at round cap:"
APPLY=0
[[ "${1:-}" == "--apply" ]] && APPLY=1
if [[ $# -gt 1 || ( $# -eq 1 && "$APPLY" -eq 0 ) ]]; then
  echo "usage: $0 [--apply]" >&2
  exit 2
fi

# The open set only: a superseded node leaves it, which is what makes a
# second run retire zero. The timeout names a wedged store instead of
# hanging the ritual.
SNAP="$(mktemp -t retire-round-cap.XXXXXX)"
trap 'rm -f "$SNAP"' EXIT
if ! timeout 120 "$FNO_BIN" backlog status --snapshot > "$SNAP" 2>/dev/null; then
  echo "retire-round-cap-findings: backlog status --snapshot failed or timed out; nothing retired" >&2
  exit 1
fi

# Candidates: OPEN nodes whose title carries the vent's prefix. jq exits 0 on
# zero matches, so an empty answer is an answer, never a pipeline loss.
# (A plain while-read array: macOS bash 3.2 has no mapfile.)
IDS=()
while IFS= read -r id; do
  [[ -n "$id" ]] && IDS+=("$id")
done < <(jq -r --arg p "$TITLE_PREFIX" \
  '.entries // [] | map(select((.title // "") | startswith($p))) | .[].id' "$SNAP")

if [[ ${#IDS[@]} -eq 0 ]]; then
  echo "retire-round-cap-findings: 0 open nodes titled '$TITLE_PREFIX ...'; nothing to retire"
  exit 0
fi

for id in "${IDS[@]}"; do
  if [[ "$APPLY" -eq 1 ]]; then
    "$FNO_BIN" backlog supersede "$OWNER_NODE" --replaces "$id" \
      --cause "$CAUSE" --surface "$SURFACE"
  else
    echo "would supersede: $id"
  fi
done

VERB="retired"; [[ "$APPLY" -eq 0 ]] && VERB="would retire"
echo "retire-round-cap-findings: $VERB ${#IDS[@]} node(s)"
