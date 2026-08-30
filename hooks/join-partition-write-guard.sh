#!/usr/bin/env bash

set -uo pipefail

# shellcheck source=lib/guard-mark.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/guard-mark.sh" 2>/dev/null || true

_approve() {
    _guard_mark join-partition-write-guard allow 2>/dev/null || true
    printf '%s\n' '{}'
    exit 0
}

PAYLOAD="$(cat)"
CWD=""
if command -v jq >/dev/null 2>&1; then
    CWD="$(printf '%s' "$PAYLOAD" | jq -er '.cwd | select(type == "string" and length > 0)' 2>/dev/null || true)"
elif command -v python3 >/dev/null 2>&1; then
    CWD="$(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
try:
    value = json.load(sys.stdin).get("cwd")
    if isinstance(value, str) and value:
        print(value)
except Exception:
    pass
' 2>/dev/null || true)"
else
    _approve
fi

[[ -n "$CWD" && -d "$CWD" ]] || _approve

# Fast path, and the reason this hook is cheap: one stat decides for every
# Edit and Write in every session that is not a joined worktree. Only a real
# `.fno/join-partition/` directory pays for the Python stage below.
[[ -d "$CWD/.fno/join-partition" ]] || _approve

PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HELPER="$PLUGIN_ROOT/hooks/helpers/join_partition_guard.py"
[[ -f "$HELPER" ]] || _approve

# The judged half needs fno's own deps (pydantic), so it runs in the plugin's
# project env - the same shape the law-authority gate uses on the Bash
# matcher. The helper prints exactly one decision JSON and always exits 0; a
# stage that cannot even run approves, because an unjailed worker is the LD3
# default, never a wedge (the OS layer still covers a real joiner).
out="$(uv run --project "$PLUGIN_ROOT/cli/pyproject.toml" python3 "$HELPER" <<< "$PAYLOAD" 2>/dev/null)"
if [[ $? -eq 0 && -n "$out" ]]; then
    # The helper owns this decision; mark which way it went before relaying it.
    if [[ "$out" == *'permissionDecision": "deny'* || "$out" == *'permissionDecision":"deny'* ]]; then
        _guard_mark join-partition-write-guard block 2>/dev/null || true
    else
        _guard_mark join-partition-write-guard allow 2>/dev/null || true
    fi
    printf '%s\n' "$out"
    exit 0
fi
_approve
