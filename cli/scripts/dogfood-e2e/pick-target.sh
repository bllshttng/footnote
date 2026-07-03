#!/usr/bin/env bash
# Query ready nodes from the graph, write chosen target to .target.json
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)

# Prefer abilities project; fall back to any project with a stderr warning.
cd "$REPO_ROOT/cli"
node=$(uv run fno-py --json graph next --project fno 2>/dev/null || true)
if [[ -z "$node" || "$node" == "null" ]]; then
  echo "WARNING: no ready node in abilities project; falling back to any project" >&2
  node=$(uv run fno-py --json graph next 2>/dev/null || true)
fi

if [[ -z "$node" || "$node" == "null" ]]; then
  echo "ERROR: no ready nodes available" >&2
  exit 3
fi

# node is a JSON object with id, plan_path, title
echo "$node" > "$SCRIPT_DIR/.target.json"
echo "Target picked: $(echo "$node" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('id','?') + ' - ' + str(d.get('title','?')))")" >&2
echo "$node"
