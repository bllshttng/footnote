#!/usr/bin/env bash
# check-workflow-manifest.sh - a workflow file cannot vanish silently.
#
# This consolidated 16 near-duplicate workflows into guards.yml and
# measured the deletion of an unmeasured smoke-dirty lane. Both were deletions
# made with a number behind them. The next deletion might not have one: this
# gate makes "a workflow file disappeared" a red check on that PR rather than
# something a reviewer has to notice in a diff of 20 deleted files. It checks
# EXISTENCE only, never workflow content - a check consolidated into another
# job (as this PR did to 16 of them) is a manifest edit, not a violation.
#
# Run: bash scripts/ci/check-workflow-manifest.sh
# Exit: 0 the live set matches the manifest, 1 a mismatch, 2 misuse.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
MANIFEST="$REPO_ROOT/scripts/ci/workflow-manifest.txt"
WORKFLOWS_DIR="$REPO_ROOT/.github/workflows"

if [[ ! -f "$MANIFEST" ]]; then
  echo "check-workflow-manifest: manifest missing at $MANIFEST" >&2
  exit 2
fi
if [[ ! -d "$WORKFLOWS_DIR" ]]; then
  echo "check-workflow-manifest: workflows dir missing at $WORKFLOWS_DIR" >&2
  exit 2
fi

LIVE="$(mktemp)"
LISTED="$(mktemp)"
trap 'rm -f "$LIVE" "$LISTED"' EXIT

(cd "$WORKFLOWS_DIR" && ls -1 -- *.yml) | sort -u > "$LIVE"
grep -vE '^[[:space:]]*(#|$)' "$MANIFEST" | sort -u > "$LISTED"

# A `while read` loop, not `mapfile` (bash 4+ only) - this script is meant
# to run unmodified on stock macOS bash (3.2), same reason `ls`, not GNU
# `find -printf`, is used above.
MISSING=()
while IFS= read -r line; do
  [[ -n "$line" ]] && MISSING+=("$line")
done < <(comm -13 "$LIVE" "$LISTED")
UNLISTED=()
while IFS= read -r line; do
  [[ -n "$line" ]] && UNLISTED+=("$line")
done < <(comm -23 "$LIVE" "$LISTED")

if [[ "${#MISSING[@]}" -eq 0 && "${#UNLISTED[@]}" -eq 0 ]]; then
  echo "check-workflow-manifest: ok ($(wc -l < "$LISTED" | tr -d ' ') workflow file(s) match the manifest)"
  exit 0
fi

if [[ "${#MISSING[@]}" -gt 0 ]]; then
  echo "check-workflow-manifest: manifest lists workflow(s) that no longer exist:" >&2
  printf '  %s\n' "${MISSING[@]}" >&2
  echo "  Remove the stale line(s) from scripts/ci/workflow-manifest.txt, or restore the file." >&2
fi
if [[ "${#UNLISTED[@]}" -gt 0 ]]; then
  echo "check-workflow-manifest: workflow(s) present but not in the manifest:" >&2
  printf '  %s\n' "${UNLISTED[@]}" >&2
  echo "  Add each to scripts/ci/workflow-manifest.txt in the same PR that added the file." >&2
fi
exit 1
