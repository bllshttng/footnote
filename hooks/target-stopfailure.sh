#!/usr/bin/env bash
# StopFailure hook: update circuit breaker counter + detect model fallback triggers
# Fires when a turn ends due to API error (429, 529, auth errors).
# Output and exit code are ignored by CC - this is informational only.
set -uo pipefail

STATE_FILE=".fno/target-state.md"

# Only act if target is active
[[ -f "$STATE_FILE" ]] || exit 0
STATUS=$(grep '^status:' "$STATE_FILE" 2>/dev/null | awk '{print $2}')
[[ "$STATUS" == "IN_PROGRESS" ]] || exit 0

INPUT=$(cat)

# Parse both fields in a single python3 call
read -r ERROR_TYPE STATUS_CODE < <(printf '%s\n' "$INPUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    print(data.get('error_type', 'unknown'), data.get('status_code', '0'))
except Exception:
    print('unknown 0')
" 2>/dev/null || echo "unknown 0")

# Circuit breaker: increment consecutive_same_error
# Normalize error into a signature for duplicate detection
ERROR_SIG=$(printf '%s' "${ERROR_TYPE}:${STATUS_CODE}" | tr '[:upper:]' '[:lower:]')

LAST_SIG=$(grep 'last_error_signature:' "$STATE_FILE" 2>/dev/null | head -1 | sed 's/.*last_error_signature: *//' | tr -d '"')

# Ensure fields exist in state file (append if missing)
if ! grep -q 'consecutive_same_error:' "$STATE_FILE" 2>/dev/null; then
    echo "consecutive_same_error: 0" >> "$STATE_FILE"
fi
if ! grep -q 'last_error_signature:' "$STATE_FILE" 2>/dev/null; then
    echo "last_error_signature: null" >> "$STATE_FILE"
fi

if [[ "$ERROR_SIG" == "$LAST_SIG" && -n "$LAST_SIG" && "$LAST_SIG" != "null" ]]; then
    # Same error - increment counter
    CURRENT=$(grep 'consecutive_same_error:' "$STATE_FILE" 2>/dev/null | head -1 | awk '{print $2}')
    CURRENT=${CURRENT:-0}
    NEW=$((CURRENT + 1))
    sed -i.bak "s/^consecutive_same_error:.*/consecutive_same_error: $NEW/" "$STATE_FILE" 2>/dev/null
    rm -f "${STATE_FILE}.bak"
else
    # Different error - reset counter, update signature
    sed -i.bak "s/^last_error_signature:.*/last_error_signature: \"$ERROR_SIG\"/" "$STATE_FILE" 2>/dev/null
    sed -i.bak "s/^consecutive_same_error:.*/consecutive_same_error: 1/" "$STATE_FILE" 2>/dev/null
    rm -f "${STATE_FILE}.bak"
fi

# Model fallback: flag rate limits for the skill to handle
if [[ "$ERROR_TYPE" == "rate_limit" || "$STATUS_CODE" == "429" ]]; then
    if grep -q 'model_fallback_needed:' "$STATE_FILE" 2>/dev/null; then
        sed -i.bak "s/^model_fallback_needed:.*/model_fallback_needed: true/" "$STATE_FILE" 2>/dev/null
    else
        echo "model_fallback_needed: true" >> "$STATE_FILE"
    fi
    rm -f "${STATE_FILE}.bak"
fi

# Log to events
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
printf '{"ts":"%s","type":"stop_failure","error_type":"%s","status_code":"%s"}\n' "$TS" "$ERROR_TYPE" "$STATUS_CODE" >> .fno/events.jsonl 2>/dev/null

exit 0
