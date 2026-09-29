#!/usr/bin/env bash
# Run after pick-target. Creates state + worktree + claims the node lockfile.
# Pass --dry-run to skip claim acquisition and worktree creation.
set -euo pipefail

DRY_RUN=false
for arg in "$@"; do
  [[ "$arg" == "--dry-run" ]] && DRY_RUN=true
done

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)
TARGET_FILE="$SCRIPT_DIR/.target.json"

if [[ ! -f "$TARGET_FILE" ]]; then
  echo "ERROR: $TARGET_FILE missing - run pick-target.sh first" >&2
  exit 3
fi

node_id=$(python3 -c "import sys,json; d=json.load(open('$TARGET_FILE')); print(d['id'])")
plan_path=$(python3 -c "import sys,json; d=json.load(open('$TARGET_FILE')); print(d.get('plan_path') or '')")
slug="e2e-$(date -u +%Y%m%dT%H%M%SZ)"

cd "$REPO_ROOT/cli"

if [[ "$DRY_RUN" == "true" ]]; then
  echo "DRY-RUN: skipping state init, worktree create, and node claim" >&2
  echo "DRY-RUN: would init state at $REPO_ROOT/.fno/e2e-state.md" >&2
  echo "DRY-RUN: would create worktree $slug" >&2
  echo "DRY-RUN: would claim node:$node_id with target-session:$slug" >&2
else
  uv run fno-py state init --type target --output "$REPO_ROOT/.fno/e2e-state.md" >&2
  uv run fno-py runtime worktree --action create --name "$slug" >&2
  uv run fno-py agents claim acquire "node:$node_id" --holder "target-session:$slug" --ttl 2h >&2
fi

echo "Session initialized: $slug" >&2
echo "{\"slug\": \"$slug\", \"node_id\": \"$node_id\", \"plan_path\": \"$plan_path\", \"dry_run\": $([[ "$DRY_RUN" == "true" ]] && echo "true" || echo "false")}"
