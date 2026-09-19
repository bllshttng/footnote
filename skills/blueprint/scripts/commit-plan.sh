#!/usr/bin/env bash
# commit-plan.sh - version one blueprint write. /blueprint mutates a plan in
# place; this commits that one file in its own git repo so `git log -- <plan>`
# holds the plan's history and each message names the node and the cause.
#
# Usage: commit-plan.sh <plan-path> <node-id> <cause>
# Output: exactly one status line on stdout:
#   committed <sha> <plan> | unchanged <plan> | unversioned <plan>
#   | failed <first git stderr line> | skipped reason=<...>
# Exit: 0 committed/unchanged/unversioned, 1 failed, 2 skipped (bad args).
# Commits the plan path ONLY (`--only`): other staged work in the repo stays
# staged and out of the commit. Hooks are never skipped.
set -uo pipefail

PLAN="${1:-}"
NODE="${2:-}"
CAUSE="${3:-}"
if [[ -z "$PLAN" || -z "$NODE" || -z "$CAUSE" ]]; then
  echo "skipped reason=missing-args"
  exit 2
fi
if [[ ! -f "$PLAN" ]]; then
  echo "skipped reason=plan-not-found"
  exit 2
fi

dir="$(cd "$(dirname "$PLAN")" && pwd -P)"
file="$dir/$(basename "$PLAN")"
if ! git -C "$dir" rev-parse --show-toplevel >/dev/null 2>&1; then
  echo "unversioned $PLAN"
  exit 0
fi

if ! err="$(git -C "$dir" add -- "$file" 2>&1)"; then
  echo "failed ${err%%$'\n'*}"
  exit 1
fi
if git -C "$dir" diff --cached --quiet -- "$file"; then
  echo "unchanged $PLAN"
  exit 0
fi

commit() {
  git -C "$dir" commit --only -q -m "blueprint($NODE): $CAUSE" -m "Node: $NODE" -- "$file" 2>&1
}
if ! err="$(commit)"; then
  if [[ "$err" == *index.lock* ]]; then
    sleep 1
    err="$(commit)" || { echo "failed ${err%%$'\n'*}"; exit 1; }
  else
    echo "failed ${err%%$'\n'*}"
    exit 1
  fi
fi
echo "committed $(git -C "$dir" rev-parse --short HEAD) $PLAN"
