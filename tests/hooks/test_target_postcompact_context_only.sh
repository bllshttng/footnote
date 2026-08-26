#!/usr/bin/env bash
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
REINJECT="$REPO_ROOT/hooks/target-postcompact-reinject.sh"
TMP="$(mktemp -d -t target-postcompact-context-XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

SID="session-context-only"
mkdir -p "$TMP/.fno"
cat > "$TMP/.fno/target-state.md" <<EOF
---
session_id: $SID
input: "x-test"
plan_path: "/tmp/plan.md"
---
graph_node_id: x-test
EOF
touch "$TMP/.fno/.handoff-armed-$SID"

OUT="$(cd "$TMP" && CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
  bash "$REINJECT" <<EOF
{"source":"compact","session_id":"$SID"}
EOF
)"

if printf '%s' "$OUT" | grep -qF '## Post-Compaction Context Reminder' \
  && ! printf '%s' "$OUT" | grep -qF 'Handoff armed'; then
  printf 'PASS: compact reinject preserves target context without arming a handoff\n'
  exit 0
fi

printf 'FAIL: expected context-only reinject, got:\n%s\n' "$OUT" >&2
exit 1
