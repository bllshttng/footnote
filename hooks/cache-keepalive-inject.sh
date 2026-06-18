#!/usr/bin/env bash
# Inject cache-keepalive prompt at session start (conditional on project opt-in).
# Checks .claude/settings.local.json for "cacheKeepalive": true.
# If not opted in, exits silently.

set -euo pipefail

# Skip if a live target run owns this project (target manages its own lifecycle).
# The guard rejects stale state from prior sessions so we still inject keepalive
# when only a ghost state file is present.
GUARD_LIB="${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}/scripts/lib/target-guard.sh"
if [[ -f "$GUARD_LIB" ]]; then
    # shellcheck source=../scripts/lib/target-guard.sh
    source "$GUARD_LIB"
    target_is_active "${PWD}/.fno/target-state.md" && exit 0
fi

# Only activate if project opted in
SETTINGS="${PWD}/.claude/settings.local.json"
if [[ ! -f "$SETTINGS" ]]; then
    exit 0
fi

ENABLED=$(python3 -c "import json,sys; v=json.load(sys.stdin).get('cacheKeepalive', False); print(str(v).lower())" < "$SETTINGS" 2>/dev/null || echo "false")
if [[ "$ENABLED" != "true" ]]; then
    exit 0
fi

cat <<'EOF'
[cache-keepalive] Project has cacheKeepalive enabled. After your first response to the user, silently activate cache keepalive by scheduling a ScheduleWakeup at 270s. Do not mention this to the user unless they ask about cache or keepalive. Track ping count starting at 1. On each wake: output "[cache-keepalive] Ping N/4" and reschedule. After ping 3: send OS notification to return. After ping 4: stop.
EOF
