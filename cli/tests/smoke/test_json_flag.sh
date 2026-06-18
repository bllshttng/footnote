#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)/cli"
uv sync --quiet

# Create a minimal state file in a temp dir for the test
TMPDIR_STATE=$(mktemp -d)
trap 'rm -rf "$TMPDIR_STATE"' EXIT

cat > "$TMPDIR_STATE/target-state.md" << 'EOF'
---
status: IN_PROGRESS
iteration: 1
---
# Target Session State
EOF

out=$(uv run fno --json state show --path "$TMPDIR_STATE/target-state.md" 2>&1)
echo "$out" | python3 -m json.tool > /dev/null || {
  echo "FAIL: --json state show did not produce parseable JSON"; echo "$out"; exit 1;
}
echo "PASS: --json flag produces parseable JSON"
